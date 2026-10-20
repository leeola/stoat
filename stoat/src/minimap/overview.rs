//! Minimap lines for the diff overview, one per overview row.
//!
//! The overview strip holds the rows the overview pane paints, a header per
//! chunk and then the chunk's diff rows, so the thumb maps the overview's scroll
//! straight onto strip lines. A diff row summarizes the side that the commit
//! preview paints as its text. That is the live side where the row has one and
//! the base side otherwise, so a deletion shows in the deleted block.

use super::{
    summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run, MAX_COLUMNS,
};
use crate::{
    diff_map::BaseHighlights,
    host::DiffStatus,
    render::commits::{preview_row_count, side_is_marked},
    review::{ReviewRow, ReviewSide},
    review_session::{DiffDocument, ReviewFile},
};
use std::ops::Range;

/// Lines the overview strip holds for `doc`, one per overview row.
pub(crate) fn row_count(doc: &DiffDocument) -> usize {
    preview_row_count(doc)
}

/// The minimap lines of the overview rows in `rows`, as the commit preview
/// paints those rows.
///
/// A chunk header row is a bar across the whole strip in the header class, the
/// unit that `n` and `p` walk. A diff row takes the lane and the content block
/// of its change, which is an addition, a modification, a move, or a deletion.
/// A side that marks no change paints as context, so its row takes no mark.
///
/// With `populations` unset, every content class stays in the plain syntax
/// block, for a terminal that reads no population blocks. A range past the
/// document's last row yields lines up to that row only.
pub(crate) fn summarize_rows(
    doc: &DiffDocument,
    rows: Range<usize>,
    table: &ClassTable,
    populations: bool,
) -> Vec<LineRuns> {
    let mut lines = Vec::with_capacity(rows.len());
    let mut row = 0;
    for file in &doc.files {
        for chunk in file.chunks.iter().filter_map(|id| doc.chunks.get(id)) {
            let chunk_rows = 1 + chunk.hunk.rows.len();
            if row + chunk_rows <= rows.start {
                row += chunk_rows;
                continue;
            }

            let header = std::iter::once(None);
            for diff_row in header.chain(chunk.hunk.rows.iter().map(Some)) {
                if row >= rows.end {
                    return lines;
                }
                if row >= rows.start {
                    lines.push(match diff_row {
                        None => header_bar(table),
                        Some(diff_row) => summarize_row(diff_row, file, table, populations),
                    });
                }
                row += 1;
            }
        }
    }
    lines
}

/// A chunk header row, a bar across the whole strip in the header class.
fn header_bar(table: &ClassTable) -> LineRuns {
    let mut line = LineRuns::default();
    line.push(Run {
        start_col: 0,
        len: MAX_COLUMNS as u8,
        class: table.edge_class(EdgeClass::Header),
        weight: 255,
    });
    line
}

/// The minimap line of one diff row of `file`.
fn summarize_row(
    row: &ReviewRow,
    file: &ReviewFile,
    table: &ClassTable,
    populations: bool,
) -> LineRuns {
    let (side, highlights, mark) = match row {
        ReviewRow::Context { right, .. } => (right, &file.buffer_highlights, RowMark::NONE),
        ReviewRow::Changed {
            left,
            right: Some(right),
        } => (
            right,
            &file.buffer_highlights,
            live_mark(right, left.is_none(), table),
        ),
        ReviewRow::Changed {
            left: Some(left),
            right: None,
        } => (left, &file.base_highlights, base_mark(left, table)),
        ReviewRow::Changed {
            left: None,
            right: None,
        } => return LineRuns::default(),
    };
    let mark = match populations {
        true => mark,
        false => RowMark {
            class_base: 0,
            ..mark
        },
    };
    let tokens = side_tokens(side, highlights.as_deref(), table);
    summarize_line(&side.text, &tokens, mark)
}

/// The mark of a live side, in the precedence the preview tints it with: a move
/// outranks an addition, which outranks a modification.
///
/// `no_base` marks a side with no base side beside it, which is an addition.
fn live_mark(side: &ReviewSide, no_base: bool, table: &ClassTable) -> RowMark {
    if !side_is_marked(side) {
        return RowMark::NONE;
    }
    let (edge, status) = if !side.moved_spans.is_empty() {
        (EdgeClass::Modified, DiffStatus::Moved)
    } else if no_base {
        (EdgeClass::Added, DiffStatus::Added)
    } else {
        (EdgeClass::Modified, DiffStatus::Modified)
    };
    RowMark {
        edge: Some(table.edge_class(edge)),
        class_base: table.population_base(status),
    }
}

/// The mark of a base side with no live side beside it, which is a deletion.
fn base_mark(side: &ReviewSide, table: &ClassTable) -> RowMark {
    if !side_is_marked(side) {
        return RowMark::NONE;
    }
    RowMark {
        edge: Some(table.edge_class(EdgeClass::Removed)),
        class_base: table.deleted_base(),
    }
}

/// The highlighted tokens of `side`'s line, resolved to classes by color.
///
/// A span in a color no theme key paints in reads as uncovered text, so it
/// drops out rather than covering its bytes with class 0.
fn side_tokens(
    side: &ReviewSide,
    highlights: Option<&BaseHighlights>,
    table: &ClassTable,
) -> Vec<LineToken> {
    let Some(spans) =
        highlights.and_then(|lines| lines.get(side.line_num.saturating_sub(1) as usize))
    else {
        return Vec::new();
    };
    let mut tokens: Vec<LineToken> = spans
        .iter()
        .filter_map(|(range, style)| {
            let class = table.class_of_fg(style.foreground);
            (class != 0).then(|| LineToken {
                range: range.clone(),
                class,
            })
        })
        .collect();
    tokens.sort_by_key(|token| token.range.start);
    tokens
}

#[cfg(test)]
mod tests {
    use super::{row_count, summarize_rows};
    use crate::{
        display_map::highlights::HighlightStyle,
        host::DiffStatus,
        minimap::{summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run},
        review::{ReviewFileInput, ReviewHunk, ReviewRow, ReviewSide},
        review_session::DiffDocument,
        theme::Theme,
    };
    use ratatui::style::Color;
    use std::{path::PathBuf, sync::Arc};

    /// A document over `files`, each a path with its base and buffer texts and
    /// no language, so the line diff cuts the chunks.
    fn doc_of(files: &[(&str, &str, &str)]) -> DiffDocument {
        let mut doc = DiffDocument::default();
        doc.add_files(
            files
                .iter()
                .map(|&(path, base, buffer)| input(path, base, buffer))
                .collect(),
        );
        doc
    }

    fn input(path: &str, base: &str, buffer: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from(path),
            rel_path: path.to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }
    }

    /// One file whose chunk holds a context row, a modified row, a context
    /// row, and a deleted row.
    fn one_file() -> DiffDocument {
        doc_of(&[("a.txt", "a\nb\nc\nd\n", "a\nB\nc\n")])
    }

    fn runs(lines: &[LineRuns]) -> Vec<Vec<Run>> {
        lines.iter().map(|line| line.as_slice().to_vec()).collect()
    }

    /// The modified row and the deleted row take the lane and the content
    /// block of their change, and the context rows take neither. Without
    /// populations the lanes stay and every content class is plain.
    #[test]
    fn rows_take_the_lane_and_block_of_their_change() {
        let table = ClassTable::from_theme(&Theme::empty());
        let doc = one_file();
        let expected = |populations: bool| {
            let mark = |edge, base| RowMark {
                edge: Some(table.edge_class(edge)),
                class_base: if populations { base } else { 0 },
            };
            let line = |text, mark| summarize_line(text, &[], mark).as_slice().to_vec();
            vec![
                vec![Run {
                    start_col: 0,
                    len: 120,
                    class: table.edge_class(EdgeClass::Header),
                    weight: 255,
                }],
                line("a", RowMark::NONE),
                line(
                    "B",
                    mark(
                        EdgeClass::Modified,
                        table.population_base(DiffStatus::Modified),
                    ),
                ),
                line("c", RowMark::NONE),
                line("d", mark(EdgeClass::Removed, table.deleted_base())),
            ]
        };

        let count = row_count(&doc);
        assert_eq!(
            [true, false].map(|populations| runs(&summarize_rows(
                &doc,
                0..count,
                &table,
                populations
            ))),
            [expected(true), expected(false)],
        );
    }

    /// Any range yields the lines the whole walk gives its rows, a range that
    /// starts past the first chunk included, and a range past the last row
    /// stops at it.
    #[test]
    fn a_range_yields_the_lines_of_its_rows() {
        let table = ClassTable::from_theme(&Theme::empty());
        let doc = doc_of(&[
            ("a.txt", "a\nb\nc\nd\n", "a\nB\nc\n"),
            ("b.txt", "x\n", "y\n"),
        ]);
        let (count, first) = (row_count(&doc), row_count(&one_file()));
        let whole = runs(&summarize_rows(&doc, 0..count, &table, true));

        let ranges = [first..count, 1..3, 2..first + 2, count - 1..count + 5];
        assert_eq!(
            ranges
                .clone()
                .map(|rows| runs(&summarize_rows(&doc, rows, &table, true))),
            ranges.map(|rows| whole[rows.start..rows.end.min(count)].to_vec()),
        );
    }

    /// A row reads the highlights of its own side at its own line, each span
    /// resolved by color into the row's content block.
    #[test]
    fn a_row_reads_the_highlights_of_its_own_side() {
        let theme = {
            let (config, _) =
                stoat_config::parse(r##"theme t { syntax.keyword.fg = "#ff0000"; }"##);
            Theme::from_config(&config.expect("the theme parses"), "t").expect("theme t exists")
        };
        let table = ClassTable::from_theme(&theme);
        let red = || {
            (
                0..1,
                HighlightStyle {
                    foreground: Some(Color::Rgb(255, 0, 0)),
                    ..HighlightStyle::default()
                },
            )
        };
        let mut doc = one_file();
        doc.files[0].buffer_highlights = Some(Arc::new(vec![vec![], vec![red()]]));
        doc.files[0].base_highlights = Some(Arc::new(vec![vec![], vec![], vec![], vec![red()]]));

        let keyword = [LineToken {
            range: 0..1,
            class: table.class_of_fg(Some(Color::Rgb(255, 0, 0))),
        }];
        let line = |text, edge, class_base| {
            let mark = RowMark {
                edge: Some(table.edge_class(edge)),
                class_base,
            };
            summarize_line(text, &keyword, mark).as_slice().to_vec()
        };
        let lines = runs(&summarize_rows(&doc, 0..row_count(&doc), &table, true));
        assert_eq!(
            [lines[2].clone(), lines[4].clone()],
            [
                line(
                    "B",
                    EdgeClass::Modified,
                    table.population_base(DiffStatus::Modified)
                ),
                line("d", EdgeClass::Removed, table.deleted_base()),
            ],
        );
    }

    /// A live side with no base side beside it is an addition, and a move
    /// outranks the addition. A side that marks no change takes no mark, on
    /// the live side and on the base side alike.
    #[test]
    fn a_move_outranks_an_addition_and_an_unmarked_side_takes_no_mark() {
        let side = |text: &str, line_num, change: bool, moved: bool| {
            let whole = |on: bool| on.then_some(0..text.len()).into_iter().collect();
            ReviewSide {
                text: text.to_string(),
                line_num,
                change_spans: whole(change),
                moved_spans: whole(moved),
                move_provenance: None,
            }
        };
        let rows = vec![
            ReviewRow::Changed {
                left: None,
                right: Some(side("added", 1, true, false)),
            },
            ReviewRow::Changed {
                left: None,
                right: Some(side("moved", 2, false, true)),
            },
            ReviewRow::Changed {
                left: Some(side("old", 1, false, false)),
                right: Some(side("new", 3, false, false)),
            },
            ReviewRow::Changed {
                left: Some(side("gone", 2, false, false)),
                right: None,
            },
        ];
        let mut doc = DiffDocument::default();
        doc.add_files_with_hunks(
            vec![input("a.txt", "old\ngone\n", "added\nmoved\nnew\n")],
            vec![vec![ReviewHunk { rows }]],
        );

        let table = ClassTable::from_theme(&Theme::empty());
        let mark = |edge, status| RowMark {
            edge: Some(table.edge_class(edge)),
            class_base: table.population_base(status),
        };
        let line = |text, mark| summarize_line(text, &[], mark).as_slice().to_vec();
        assert_eq!(
            runs(&summarize_rows(&doc, 1..5, &table, true)),
            [
                line("added", mark(EdgeClass::Added, DiffStatus::Added)),
                line("moved", mark(EdgeClass::Modified, DiffStatus::Moved)),
                line("new", RowMark::NONE),
                line("gone", RowMark::NONE),
            ],
        );
    }
}
