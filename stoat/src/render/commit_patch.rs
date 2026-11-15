//! The Commit Preview's patch, as the BlamePreview board draws it.
//!
//! The patch reads as one unified diff over every file of a commit. Each file
//! opens with a File Header. A fold row before a hunk counts the unchanged
//! lines above the hunk and names its Scope. Each line takes one row with its
//! old and new numbers, a lane in its change color, and its code on the row
//! tint. The rows build from a [`DiffDocument`] alone, so the overlay needs no
//! editor, no buffer, and no pool.

use crate::{
    diff_changes::ExcerptStats,
    display_map::{FoldScope, ScopeMark, DEFAULT_TAB_SIZE},
    render::{
        chrome,
        commits::{self, LineKind, PreviewDials},
        diff_changes, paint,
        review::{self, DiffTints},
        text,
    },
    review::{ReviewRow, ReviewSide},
    review_session::{DiffDocument, ReviewChunkId},
    theme::{scope, Theme, TypeRole},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
use std::{fmt::Write, path::Path, sync::Arc};
use stoat_widgets::{
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};

/// The cells each line number takes, which hold three digits and a gap.
const NUMBER_CELLS: u16 = 4;

/// The column the old line number starts in, past the lane's cell.
const OLD_X: u16 = 1;

/// The column the new line number starts in.
const NEW_X: u16 = OLD_X + NUMBER_CELLS;

/// The column the rule between the numbers and the code takes.
const RULE_X: u16 = NEW_X + NUMBER_CELLS;

/// The column the code starts in.
const TEXT_X: u16 = RULE_X + 1;

/// The Scope of a fold row over a hunk that no item holds, which draws the
/// row's band and its count with no chip.
static NO_SCOPE: FoldScope = FoldScope {
    segments: Vec::new(),
    split: None,
    end_of_file: false,
    mark: ScopeMark::None,
};

/// One row of a commit's patch, as the Commit Preview shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatchRow {
    /// The File Header of file `file` of the document.
    Header { file: usize },
    /// The fold row over the `hidden` unchanged lines above a hunk, which
    /// names the hunk's Scope.
    Fold {
        hidden: u32,
        scope: Option<Arc<FoldScope>>,
    },
    /// One line of a hunk.
    Line(PatchLine),
}

/// One line of a hunk in a commit's patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PatchLine {
    /// The index of the line's file in the document.
    pub(crate) file: usize,
    /// The line's 1-based number in the parent's text, or `None` for an added
    /// line.
    pub(crate) old: Option<u32>,
    /// The line's 1-based number in the commit's text, or `None` for a
    /// removed line.
    pub(crate) new: Option<u32>,
    pub(crate) kind: LineKind,
    /// The chunk the line comes from.
    pub(crate) chunk: ReviewChunkId,
    /// The index in the chunk's hunk of the [`ReviewRow`] the line comes from.
    /// A removed line shows the row's left side, and any other line its right
    /// side.
    pub(crate) row: usize,
}

/// The rows of `doc`'s patch, file by file.
///
/// Each file opens with its [`PatchRow::Header`]. A [`PatchRow::Fold`] goes
/// before each hunk that unchanged lines part from the file's top or from the
/// hunk before it, and none follows the last hunk. A run of changed rows lists
/// its removed lines and then its added lines, as a unified diff does.
pub(crate) fn patch_rows(doc: &DiffDocument) -> Vec<PatchRow> {
    let mut rows = Vec::new();
    for (file, entry) in doc.files.iter().enumerate() {
        rows.push(PatchRow::Header { file });

        // The last line of the commit's text that a hunk before this one shows.
        let mut shown = 0;
        for &id in &entry.chunks {
            let Some(chunk) = doc.chunks.get(&id) else {
                continue;
            };
            let new_lines = || chunk.hunk.rows.iter().filter_map(new_line);
            let hidden = new_lines()
                .min()
                .map_or(0, |first| first.saturating_sub(shown + 1));
            if hidden > 0 {
                rows.push(PatchRow::Fold {
                    hidden,
                    scope: chunk.scope.clone(),
                });
            }
            shown = new_lines().max().unwrap_or(shown);
            push_lines(&mut rows, file, id, &chunk.hunk.rows);
        }
    }
    rows
}

/// The index in `rows` of the row that holds line `line`, 0-based, of the
/// commit's version of the file at `path`, or `None` when no hunk shows that
/// line.
pub(crate) fn origin_row(
    doc: &DiffDocument,
    rows: &[PatchRow],
    path: &Path,
    line: u32,
) -> Option<usize> {
    let file = doc
        .files
        .iter()
        .position(|file| Path::new(&file.rel_path) == path)?;
    rows.iter().position(|row| {
        matches!(row, PatchRow::Line(shown) if shown.file == file && shown.new == Some(line + 1))
    })
}

/// The first scroll of a patch whose origin is row `origin`, for a body of
/// `body_rows` rows.
///
/// The scroll puts the File Header of the origin's file at the top when the
/// origin then shows, and otherwise puts the origin one third down the body.
pub(crate) fn origin_scroll(rows: &[PatchRow], origin: usize, body_rows: usize) -> usize {
    let header = rows.get(..=origin).and_then(|above| {
        above
            .iter()
            .rposition(|row| matches!(row, PatchRow::Header { .. }))
    });
    match header {
        Some(header) if origin - header < body_rows => header,
        _ => origin.saturating_sub(body_rows / 3),
    }
}

/// The file count and the added and removed lines of `doc`, as its File
/// Headers count them.
///
/// The Commit Preview's meta line reads this for a patch whose build carries
/// no change summary, which is the patch of a merge against a parent other
/// than the first.
pub(crate) fn patch_summary(doc: &DiffDocument) -> (usize, u32, u32) {
    (0..doc.files.len())
        .map(|file| file_stats(doc, file))
        .fold((doc.files.len(), 0, 0), |(files, added, removed), stats| {
            (files, added + stats.added, removed + stats.removed)
        })
}

/// Paint `rows`, the rows of `doc`'s patch from the scroll on, into `area`,
/// one row per screen row from its top.
///
/// A File Header names its file and the lines the file adds and removes on the
/// raised surface, between two hairlines under a live scene. A fold row takes
/// `diff.fold`'s surface with its count at the left in `diff.fold.range` and
/// the hunk's Scope as a chip in the middle. A line row takes a lane in its
/// change color, its old and new numbers, a rule, and its code, on
/// `vcs.commit.row.added` or `vcs.commit.row.removed` for a changed line. The
/// code takes the token colors of the commit's text, and a removed line takes
/// `ui.text.dim` with none, as the commits view's preview paints them.
pub(crate) fn render_commit_patch(
    doc: &DiffDocument,
    rows: &[PatchRow],
    theme: &Theme,
    area: Rect,
    dials: PreviewDials,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let shown = &rows[..rows.len().min(usize::from(area.height))];
    let row_rect = |index: usize| Rect::new(area.x, area.y + index as u16, area.width, 1);

    let headers = shown
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, PatchRow::Header { .. }))
        .map(|(index, _)| row_rect(index));
    diff_changes::emit_header_hairlines(headers, theme, scene);

    let mut lines = LinePainter::new(doc, theme, dials, scene.live());
    for (index, row) in shown.iter().enumerate() {
        let rect = row_rect(index);
        match row {
            PatchRow::Header { file } => {
                let runs = diff_changes::header_runs(
                    &doc.files[*file].rel_path,
                    file_stats(doc, *file),
                    theme,
                );
                diff_changes::paint_header(buf, rect, &runs, theme);
            },
            PatchRow::Fold { hidden, scope } => {
                paint_fold_row(*hidden, scope.as_deref(), theme, rect, buf, scene);
            },
            PatchRow::Line(line) => lines.paint(line, rect, buf, scene),
        }
    }
    lines.numbers.flush(scene);

    let divider = theme.get(scope::UI_DIVIDER);
    let mut y = area.y;
    for run in shown.chunk_by(|a, b| is_line(a) == is_line(b)) {
        if is_line(&run[0]) {
            chrome::vline(buf, area.x + RULE_X, y, run.len() as u16, divider, scene);
        }
        y += run.len() as u16;
    }
}

/// The line rows of one paint, with the styles they share and the batch their
/// numbers draw in under a live scene.
struct LinePainter<'a> {
    doc: &'a DiffDocument,
    theme: &'a Theme,
    dials: PreviewDials,
    tints: Option<DiffTints>,
    number_scale: u16,
    /// The numbers' color under a live scene, which draws them as runs, or
    /// `None` to draw them in cells.
    number_color: Option<[u8; 3]>,
    numbers: TextRunBatch,
    scratch: String,
}

impl<'a> LinePainter<'a> {
    fn new(doc: &'a DiffDocument, theme: &'a Theme, dials: PreviewDials, live: bool) -> Self {
        Self {
            doc,
            theme,
            dials,
            tints: review::resolve_diff_tints(theme),
            number_scale: theme.type_scale(TypeRole::Small),
            number_color: paint::style_rgb(theme.get(scope::UI_TEXT_MUTED).fg).filter(|_| live),
            numbers: TextRunBatch::default(),
            scratch: String::new(),
        }
    }

    /// Paint `line` into `rect`.
    fn paint(&mut self, line: &PatchLine, rect: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        let Some(side) = self.side(line) else {
            return;
        };
        let row_bg = match line.kind {
            LineKind::Context => None,
            LineKind::Added => self.theme.get(scope::VCS_COMMIT_ROW_ADDED).bg,
            LineKind::Removed => self.theme.get(scope::VCS_COMMIT_ROW_REMOVED).bg,
        };
        if let Some(bg) = row_bg {
            for x in rect.x..rect.right() {
                buf[(x, rect.y)].set_bg(bg);
            }
        }
        commits::paint_lane(
            line.kind,
            side,
            self.theme,
            Rect::new(rect.x, rect.y, 1, 1),
            buf,
            scene,
        );
        for (x, number) in [(OLD_X, line.old), (NEW_X, line.new)] {
            if let Some(number) = number {
                self.number(rect.x + x, rect.y, number, buf);
            }
        }

        // A line's number is 1-based, and the highlight index is 0-based. An
        // off syntax dial withholds the spans, as it does in the commits view.
        let tokens = self.doc.files[line.file]
            .buffer_highlights
            .as_deref()
            .filter(|_| self.dials.syntax)
            .and_then(|lines| lines.get(side.line_num.saturating_sub(1) as usize))
            .map_or(&[][..], Vec::as_slice);
        let (tokens, fallback, soften_row) = match line.kind {
            LineKind::Removed => (&[][..], self.theme.get(scope::UI_TEXT_DIM), None),
            LineKind::Added => (tokens, Style::default(), None),
            LineKind::Context => (
                tokens,
                Style::default(),
                self.tints.as_ref().map(|tints| tints.bg),
            ),
        };
        review::paint_base_row(
            buf,
            rect.x + TEXT_X,
            rect.y,
            &side.text,
            usize::from(rect.width.saturating_sub(TEXT_X)),
            DEFAULT_TAB_SIZE,
            tokens,
            fallback,
            &[],
            self.tints.as_ref(),
            soften_row,
            None,
            self.dials.paint,
            None,
        );
    }

    /// The side of its review row that `line` shows.
    fn side(&self, line: &PatchLine) -> Option<&'a ReviewSide> {
        let row = self.doc.chunks.get(&line.chunk)?.hunk.rows.get(line.row)?;
        match (row, line.kind) {
            (ReviewRow::Changed { left, .. }, LineKind::Removed) => left.as_ref(),
            (ReviewRow::Changed { right, .. }, _) => right.as_ref(),
            (ReviewRow::Context { right, .. }, _) => Some(right),
        }
    }

    /// Draw `number` right-aligned in the number column at column `x` of row
    /// `y`, as a run under a live scene and in cells otherwise.
    fn number(&mut self, x: u16, y: u16, number: u32, buf: &mut Buffer) {
        let Some(color) = self.number_color else {
            paint::render_side_num(
                buf,
                &mut self.scratch,
                x,
                y,
                number,
                self.theme.get(scope::UI_TEXT_MUTED),
                usize::from(NUMBER_CELLS - 1),
            );
            return;
        };

        self.scratch.clear();
        write!(self.scratch, "{number}").expect("writing to a String is infallible");
        let advance = text_run::advance_sixteenths(self.scratch.len(), self.number_scale);
        self.numbers.push(
            Rect::new(x, y, 1, 1),
            TextRun {
                col: ((NUMBER_CELLS - 1) * 16).saturating_sub(advance) as i16,
                row: 0,
                scale: self.number_scale,
                color,
                bg: None,
                text: &self.scratch,
                bold: false,
                italic: false,
                follow: 0,
                anchor: None,
            },
        );
    }
}

/// The commit's number of `row`, or `None` for a removed line.
fn new_line(row: &ReviewRow) -> Option<u32> {
    match row {
        ReviewRow::Context { right, .. } => Some(right.line_num),
        ReviewRow::Changed { right, .. } => right.as_ref().map(|side| side.line_num),
    }
}

/// Push a [`PatchRow::Line`] for each line of `hunk`, a run of changed rows as
/// its removed lines and then its added lines.
fn push_lines(rows: &mut Vec<PatchRow>, file: usize, chunk: ReviewChunkId, hunk: &[ReviewRow]) {
    let changed = |row: &ReviewRow| matches!(row, ReviewRow::Changed { .. });
    let mut start = 0;
    for run in hunk.chunk_by(|a, b| changed(a) && changed(b)) {
        let line = |offset: usize, old, new, kind| {
            PatchRow::Line(PatchLine {
                file,
                old,
                new,
                kind,
                chunk,
                row: start + offset,
            })
        };
        for (offset, review_row) in run.iter().enumerate() {
            if let ReviewRow::Changed {
                left: Some(side), ..
            } = review_row
            {
                rows.push(line(offset, Some(side.line_num), None, LineKind::Removed));
            }
        }
        for (offset, review_row) in run.iter().enumerate() {
            match review_row {
                ReviewRow::Context { left, right } => rows.push(line(
                    offset,
                    Some(left.line_num),
                    Some(right.line_num),
                    LineKind::Context,
                )),
                ReviewRow::Changed {
                    right: Some(side), ..
                } => {
                    rows.push(line(offset, None, Some(side.line_num), LineKind::Added));
                },
                ReviewRow::Changed { right: None, .. } => {},
            }
        }
        start += run.len();
    }
}

/// The lines file `file` of `doc` adds and removes, which its header counts.
fn file_stats(doc: &DiffDocument, file: usize) -> ExcerptStats {
    doc.files[file]
        .chunks
        .iter()
        .filter_map(|id| doc.chunks.get(id))
        .flat_map(|chunk| &chunk.hunk.rows)
        .fold(ExcerptStats::default(), |mut stats, row| {
            if let ReviewRow::Changed { left, right } = row {
                stats.added += u32::from(right.is_some());
                stats.removed += u32::from(left.is_some());
            }
            stats
        })
}

/// Paint a fold row over `hidden` lines into `rect`, with `hunk_scope` as its
/// chip.
///
/// The count draws as a run under a live scene, since the row's hatch is a bar
/// and a bar draws over cell glyphs.
fn paint_fold_row(
    hidden: u32,
    hunk_scope: Option<&FoldScope>,
    theme: &Theme,
    rect: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let live = scene.live();
    review::paint_scope_row(
        hunk_scope.unwrap_or(&NO_SCOPE),
        rect,
        theme,
        theme.get(scope::UI_TEXT),
        buf,
        live.then_some(&mut *scene),
        false,
    );

    let count = review::fold_row_placeholder(hidden).text;
    let style = theme.get(scope::DIFF_FOLD_RANGE);
    match paint::style_rgb(style.fg).filter(|_| live) {
        Some(color) => TextRun {
            col: 16,
            row: 0,
            scale: theme.type_scale(TypeRole::Small),
            color,
            bg: None,
            text: &count,
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(rect.x, rect.y, 1, 1), buf, scene),
        None => text::write_str_clipped(buf, rect.x + 1, rect.y, &count, style, rect.right()),
    }
}

/// Whether `row` is a line of a hunk.
fn is_line(row: &PatchRow) -> bool {
    matches!(row, PatchRow::Line(_))
}

#[cfg(test)]
mod tests {
    use super::{origin_row, origin_scroll, patch_rows, render_commit_patch, PatchLine, PatchRow};
    use crate::{
        display_map::{FoldScope, ScopeMark},
        render::commits::{LineKind, PreviewDials},
        review::ReviewFileInput,
        review_session::{DiffDocument, ReviewChunkId},
        theme::Theme,
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use std::{
        iter,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_language::ScopeSegment;
    use stoat_widgets::ApcScene;

    /// The rows of a document of two files, the first with two hunks 20 lines
    /// apart and the second a new file. Each file opens with its header, a fold
    /// counts the lines above each hunk of the first file and carries the
    /// hunk's Scope, and a changed line lists its removed side above its added
    /// sides.
    #[test]
    fn patch_rows_lay_out_each_file_as_a_unified_diff() {
        let doc = two_files();
        let [first, second] = [doc.files[0].chunks[0], doc.files[0].chunks[1]];
        let new_file = doc.files[1].chunks[0];
        let lines = |chunk, rows: &[(Option<u32>, Option<u32>, LineKind, usize)]| {
            rows.iter()
                .map(|&(old, new, kind, row)| line(0, chunk, row, old, new, kind))
                .collect::<Vec<_>>()
        };
        use LineKind::{Added, Context, Removed};

        let want = [
            vec![
                PatchRow::Header { file: 0 },
                PatchRow::Fold {
                    hidden: 1,
                    scope: Some(Arc::new(alpha_scope())),
                },
            ],
            lines(
                first,
                &[
                    (Some(2), Some(2), Context, 0),
                    (Some(3), Some(3), Context, 1),
                    (Some(4), Some(4), Context, 2),
                    (Some(5), None, Removed, 3),
                    (None, Some(5), Added, 3),
                    (None, Some(6), Added, 4),
                    (Some(6), Some(7), Context, 5),
                    (Some(7), Some(8), Context, 6),
                    (Some(8), Some(9), Context, 7),
                ],
            ),
            vec![PatchRow::Fold {
                hidden: 20,
                scope: None,
            }],
            lines(
                second,
                &[
                    (Some(29), Some(30), Context, 0),
                    (Some(30), Some(31), Context, 1),
                    (Some(31), Some(32), Context, 2),
                    (Some(32), None, Removed, 3),
                    (None, Some(33), Added, 3),
                    (Some(33), Some(34), Context, 4),
                    (Some(34), Some(35), Context, 5),
                    (Some(35), Some(36), Context, 6),
                ],
            ),
            vec![
                PatchRow::Header { file: 1 },
                line(1, new_file, 0, None, Some(1), Added),
            ],
        ]
        .concat();
        assert_eq!(patch_rows(&doc), want);
    }

    /// Without a live scene the rows are cells. Each File Header centers its
    /// file's name and counts, and the fold row reads its count from its second
    /// cell with the hunk's Scope centered. A changed line's removed side shows
    /// above its added sides, with the old number in the first column and the
    /// new number in the second, each on its tint.
    #[test]
    fn the_cell_rows_read_the_header_the_count_and_both_numbers() {
        let doc = two_files();
        let theme = {
            let src = r##"theme t {
                vcs.commit.row.added.bg = "#18241b";
                vcs.commit.row.removed.bg = "#2a1b1d";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("the theme parses"), "t").expect("theme t loads")
        };
        let area = Rect::new(0, 0, 100, 22);
        let mut buf = Buffer::empty(area);
        render_commit_patch(
            &doc,
            &patch_rows(&doc),
            &theme,
            area,
            PreviewDials::untinted(),
            &mut buf,
            &mut ApcScene::new(),
        );

        let rows: Vec<String> = (0..9)
            .chain(20..22)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let tints: Vec<Color> = (2..9).map(|y| buf[(60, y)].bg).collect();
        let (removed, added) = (Color::Rgb(0x2a, 0x1b, 0x1d), Color::Rgb(0x18, 0x24, 0x1b));
        assert_eq!(
            (rows, tints),
            (
                vec![
                    format!("{:^100}", "a.txt +3 \u{2212}2"),
                    format!("{:<46}{:<54}", " \u{22ef} 1 line", "fn alpha"),
                    format!("{:<100}", "   2   2 \u{2502}l2"),
                    format!("{:<100}", "   3   3 \u{2502}l3"),
                    format!("{:<100}", "   4   4 \u{2502}l4"),
                    format!("{:<100}", "\u{258e}  5     \u{2502}l5"),
                    format!("{:<100}", "\u{258e}      5 \u{2502}L5"),
                    format!("{:<100}", "\u{258e}      6 \u{2502}new"),
                    format!("{:<100}", "   6   7 \u{2502}l6"),
                    format!("{:^100}", "b.txt +1"),
                    format!("{:<100}", "\u{258e}      1 \u{2502}new"),
                ],
                vec![
                    Color::Reset,
                    Color::Reset,
                    Color::Reset,
                    removed,
                    added,
                    added,
                    Color::Reset
                ],
            ),
        );
    }

    /// The origin row is the line row of the origin's file whose commit number
    /// is the origin's line plus one. A line that no hunk shows has none.
    #[test]
    fn origin_row_finds_the_line_by_its_commit_number() {
        let doc = two_files();
        let rows = patch_rows(&doc);
        let find = |path: &str, line| origin_row(&doc, &rows, Path::new(path), line);
        assert_eq!(
            [
                find("a.txt", 5),
                find("b.txt", 0),
                find("a.txt", 20),
                find("b.txt", 4)
            ],
            [Some(7), Some(21), None, None],
        );
    }

    /// The first scroll tops the body with the nearest header above the origin
    /// when the origin then shows, and otherwise puts the origin a third of the
    /// way down the body.
    #[test]
    fn origin_scroll_tops_the_header_or_thirds_the_origin() {
        let fold = || PatchRow::Fold {
            hidden: 1,
            scope: None,
        };
        let rows: Vec<PatchRow> = [
            PatchRow::Header { file: 0 },
            fold(),
            PatchRow::Header { file: 1 },
        ]
        .into_iter()
        .chain(iter::repeat_with(fold).take(70))
        .collect();
        assert_eq!(
            [
                origin_scroll(&rows, 11, 40),
                origin_scroll(&rows, 41, 40),
                origin_scroll(&rows, 42, 40),
                origin_scroll(&rows, 62, 40),
            ],
            [2, 2, 29, 49],
        );
    }

    /// A document of `a.txt`, whose 40 lines change at line 5, gain a line
    /// after it, and change at line 32, and `b.txt`, a new file of one line.
    /// The first hunk of `a.txt` carries [`alpha_scope`].
    fn two_files() -> DiffDocument {
        let base: String = (1..=40).map(|n| format!("l{n}\n")).collect();
        let buffer: String = (1..=40)
            .map(|n| match n {
                5 => "L5\nnew\n".to_string(),
                32 => "L32\n".to_string(),
                _ => format!("l{n}\n"),
            })
            .collect();
        let mut doc = DiffDocument::default();
        doc.add_files(vec![
            input("a.txt", &base, &buffer),
            input("b.txt", "", "new\n"),
        ]);
        let first = doc.files[0].chunks[0];
        if let Some(chunk) = doc.chunks.get_mut(&first) {
            chunk.scope = Some(Arc::new(alpha_scope()));
        }
        doc
    }

    /// The Scope of a hunk inside `fn alpha`.
    fn alpha_scope() -> FoldScope {
        FoldScope {
            segments: vec![ScopeSegment {
                keyword: Some("fn".to_string()),
                name: "alpha".to_string(),
                item_range: 0..0,
                name_range: 0..0,
            }],
            split: None,
            end_of_file: false,
            mark: ScopeMark::None,
        }
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

    fn line(
        file: usize,
        chunk: ReviewChunkId,
        row: usize,
        old: Option<u32>,
        new: Option<u32>,
        kind: LineKind,
    ) -> PatchRow {
        PatchRow::Line(PatchLine {
            file,
            old,
            new,
            kind,
            chunk,
            row,
        })
    }
}
