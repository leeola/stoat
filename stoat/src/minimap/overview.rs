//! Minimap lines for the diff overview, one per overview row.
//!
//! The overview strip holds the rows the overview pane paints, a header per
//! chunk and then the chunk's diff rows, so the thumb maps the overview's scroll
//! straight onto strip lines. A diff row summarizes the side that the commit
//! preview paints as its text. That is the live side where the row has one and
//! the base side otherwise, so a deletion shows in the deleted block.
//!
//! A workspace holds one strip for its overviews, filled from its overview
//! document a slice per frame, as a freshly opened file fills its own strip.

use super::{
    emit::{convert_minimap_runs, POPULATION_PALETTE_PROTOCOL, WEIGHTED_RUN_PROTOCOL},
    summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run, MAX_COLUMNS,
    SYNC_SLICE,
};
use crate::{
    app::Stoat,
    diff_map::BaseHighlights,
    host::DiffStatus,
    overview::OverviewId,
    render::commits::{preview_row_count, side_is_marked},
    review::{ReviewRow, ReviewSide},
    review_session::{DiffDocument, ReviewFile},
    smooth_scroll::non_pane_pool,
    workspace::WorkspaceId,
};
use std::{iter, mem, ops::Range, sync::Arc};
use stoat_widgets::pool::MinimapWindowInputs;
use stoatty_protocol::command::{self, MinimapLinesCommand};

/// The minimap strip of a workspace's overviews, kept in step with the
/// workspace's overview document.
///
/// The terminal holds the lines already sent. A new document replaces them,
/// and the first splice of the new lines removes every line the terminal still
/// holds.
pub(crate) struct OverviewStrip {
    content_id: u32,
    /// The document the strip's lines summarize, or fill from.
    synced: Option<Arc<DiffDocument>>,
    /// Whether the lines carry population blocks, which the terminal's protocol
    /// decides.
    populations: bool,
    /// Rows of [`Self::synced`].
    total: usize,
    /// Rows of [`Self::synced`] already sent.
    built_upto: usize,
    /// Lines of a replaced document that the next splice removes.
    pending_removed: usize,
}

impl OverviewStrip {
    pub(crate) fn new(content_id: u32) -> Self {
        Self {
            content_id,
            synced: None,
            populations: false,
            total: 0,
            built_upto: 0,
            pending_removed: 0,
        }
    }

    /// The id naming the strip's content store on the terminal.
    pub(crate) fn content_id(&self) -> u32 {
        self.content_id
    }

    /// Whether rows of the synced document are left to send.
    pub(crate) fn build_pending(&self) -> bool {
        self.built_upto < self.total
    }
}

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

            let header = iter::once(None);
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

/// Bring the overview strip of workspace `ws_id` in step with the workspace's
/// overview document, and append at most one slice of lines to `out`.
///
/// A new document replaces the strip's lines, and so does a protocol that
/// changes whether lines carry population blocks. Each call sends the next
/// slice, so a long document fills over idle frames. A document with no rows
/// empties the strip in one splice.
pub(crate) fn sync_overview_strip(stoat: &mut Stoat, ws_id: WorkspaceId, out: &mut Vec<u8>) {
    let populations = stoat.stoatty_protocol >= POPULATION_PALETTE_PROTOCOL;
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
    let Stoat {
        workspaces,
        overview_strips,
        minimap_class_table,
        ..
    } = stoat;
    let (Some(ws), Some(strip)) = (workspaces.get(ws_id), overview_strips.get_mut(&ws_id)) else {
        return;
    };

    let doc = ws.overview_doc.as_ref().and_then(|built| built.doc.clone());
    let same = match (&doc, &strip.synced) {
        (Some(doc), Some(synced)) => Arc::ptr_eq(doc, synced),
        (doc, synced) => doc.is_none() && synced.is_none(),
    };
    if !same || strip.populations != populations {
        // The terminal holds the lines already sent, and the lines an earlier
        // replacement has yet to remove when it sent none.
        strip.pending_removed += strip.built_upto;
        strip.total = doc.as_deref().map_or(0, row_count);
        strip.built_upto = 0;
        strip.synced = doc;
        strip.populations = populations;
    }
    if strip.built_upto == strip.total && strip.pending_removed == 0 {
        return;
    }

    let rows = strip.built_upto..(strip.built_upto + SYNC_SLICE as usize).min(strip.total);
    let lines = strip.synced.as_deref().map_or_else(Vec::new, |doc| {
        summarize_rows(doc, rows.clone(), minimap_class_table, populations)
    });
    let splice = MinimapLinesCommand {
        content_id: strip.content_id,
        start: rows.start as u32,
        removed: mem::take(&mut strip.pending_removed) as u32,
        lines: lines.into_iter().map(convert_minimap_runs).collect(),
    };
    strip.built_upto = rows.end;
    match weighted {
        true => command::encode_minimap_runs_into(out, &splice),
        false => command::encode_minimap_lines_into(out, &splice),
    }
}

/// Place the thumb of overview `id` in workspace `ws_id` on strip `strip_id`,
/// over the rows that the overview's last paint showed.
///
/// The thumb passes the change gate the editor thumbs pass, under a pool id of
/// its own. A strip that shows the overview's thumb and then an editor's
/// therefore gets the editor's thumb back, though the editor's view never
/// moved.
pub(crate) fn emit_overview_thumb(
    stoat: &mut Stoat,
    ws_id: WorkspaceId,
    id: OverviewId,
    strip_id: u32,
    out: &mut Vec<u8>,
) {
    let Some(state) = stoat
        .workspaces
        .get(ws_id)
        .and_then(|ws| ws.overviews.get(id))
    else {
        return;
    };
    let top = state.scroll_top as f32;
    let rows = state.viewport_rows.max(1) as u32;
    let visible = rows.min(u16::MAX as u32) as u16;
    stoat.smooth_scroll.emit_minimap_view(
        out,
        strip_id,
        non_pane_pool::OVERVIEW_THUMB,
        MinimapWindowInputs::new(top, 0, rows),
        || (top, visible),
    );
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
        action_handlers::dispatch,
        apc_emit,
        display_map::highlights::HighlightStyle,
        host::DiffStatus,
        minimap::{
            emit::{self, convert_minimap_runs},
            summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run, SYNC_SLICE,
        },
        overview::OverviewStamp,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::SINGLE_MINIMAP_STRIP_ID,
        review::{ReviewFileInput, ReviewHunk, ReviewRow, ReviewSide},
        review_session::DiffDocument,
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::Theme,
    };
    use ratatui::style::Color;
    use std::{
        ops::Range,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_config::MinimapMode;
    use stoatty_protocol::command::{Command, LineSummary};
    use tokio::sync::mpsc::UnboundedReceiver;

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

    /// `count` changed one-line files under a focused overview, on a stoatty of
    /// protocol 13, in a window short enough that the overview scrolls.
    fn strip_harness(count: usize) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = TestHarness::with_size(100, 10);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        let files: Vec<(String, String, String)> = (0..count)
            .map(|i| (format!("f{i:03}.txt"), format!("{i}\n"), format!("{i}!\n")))
            .collect();
        let files: Vec<(&str, &str, &str)> = files
            .iter()
            .map(|(path, base, buffer)| (path.as_str(), base.as_str(), buffer.as_str()))
            .collect();
        h.stage_review_scenario("/repo", &files);
        h.open_file(Path::new("/repo/f000.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffOverview);
        h.settle();
        (h, rx)
    }

    /// Paint a frame, emit its scene and its minimap frames, and decode what
    /// went out.
    fn frame(h: &mut TestHarness, rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<Command> {
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit::emit_minimap(&mut h.stoat);
        drain_apc(rx)
    }

    type Splice = (u32, u32, u32, Vec<LineSummary>);

    /// The splices and the thumbs among `commands`, each in order.
    fn strip_frames(commands: &[Command]) -> (Vec<Splice>, Vec<(u32, u32, u16)>) {
        let mut splices = Vec::new();
        let mut views = Vec::new();
        for command in commands {
            match command {
                Command::MinimapLines(splice) => splices.push((
                    splice.content_id,
                    splice.start,
                    splice.removed,
                    splice.lines.clone(),
                )),
                Command::MinimapView(view) => {
                    views.push((view.strip_id, view.top_256, view.visible_lines))
                },
                _ => {},
            }
        }
        (splices, views)
    }

    fn strip_doc(h: &TestHarness) -> Arc<DiffDocument> {
        h.stoat
            .active_workspace()
            .overview_doc
            .as_ref()
            .and_then(|built| built.doc.clone())
            .expect("a built overview document")
    }

    fn strip_id(h: &TestHarness) -> u32 {
        h.stoat.overview_strips[&h.stoat.active_workspace].content_id()
    }

    /// The overview's scroll and its painted rows.
    fn view_of(h: &TestHarness) -> (usize, usize) {
        let state = h
            .stoat
            .active_workspace()
            .focused_overview()
            .expect("a focused overview");
        (state.scroll_top, state.viewport_rows)
    }

    /// Every line of `doc` in the wire form the strip sends.
    fn wire_lines(h: &TestHarness, doc: &DiffDocument, populations: bool) -> Vec<LineSummary> {
        summarize_rows(
            doc,
            0..row_count(doc),
            &h.stoat.minimap_class_table,
            populations,
        )
        .into_iter()
        .map(convert_minimap_runs)
        .collect()
    }

    /// Replace `range` in the open buffer of f000.txt with `text`, then let the
    /// focused overview's rebuild land.
    fn edit_and_rebuild(h: &mut TestHarness, range: Range<usize>, text: &str) {
        let ws = h.stoat.active_workspace();
        let buffer_id = ws
            .buffers
            .id_for_path(Path::new("/repo/f000.txt"))
            .expect("an open buffer");
        ws.buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("buffer poisoned")
            .edit(range, text);
        h.stoat.drive_background();
        h.settle();
    }

    /// The first frame fills the strip with every overview row in one splice,
    /// and places the thumb over the painted rows on the band.
    #[test]
    fn the_first_frame_fills_the_strip_and_places_its_thumb() {
        let (mut h, mut rx) = strip_harness(8);
        let frames = strip_frames(&frame(&mut h, &mut rx));
        let (_, rows) = view_of(&h);
        assert_eq!(
            frames,
            (
                vec![(strip_id(&h), 0, 0, wire_lines(&h, &strip_doc(&h), true))],
                vec![(SINGLE_MINIMAP_STRIP_ID, 0, rows as u16)],
            ),
        );
    }

    /// A page moves the thumb alone, and a dial step sends neither lines nor a
    /// thumb.
    #[test]
    fn a_page_moves_the_thumb_and_a_dial_step_sends_nothing() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);

        dispatch(&mut h.stoat, &stoat_action::OverviewPageDown);
        let paged = strip_frames(&frame(&mut h, &mut rx));
        let (top, rows) = view_of(&h);
        dispatch(&mut h.stoat, &stoat_action::DiffTintUp);
        let dialed = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            (top > 0, paged, dialed),
            (
                true,
                (
                    vec![],
                    vec![(SINGLE_MINIMAP_STRIP_ID, top as u32 * 256, rows as u16)]
                ),
                (vec![], vec![]),
            ),
        );
    }

    /// A rebuilt document replaces every line the strip holds, from its first
    /// row.
    #[test]
    fn a_rebuilt_document_replaces_the_strips_lines() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let held = row_count(&strip_doc(&h)) as u32;

        edit_and_rebuild(&mut h, 0..0, "x\n");
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            splices,
            [(strip_id(&h), 0, held, wire_lines(&h, &strip_doc(&h), true))]
        );
    }

    /// A document past one slice fills over several frames, and the frames
    /// between keep the build pending.
    #[test]
    fn a_long_document_fills_a_slice_per_frame() {
        let (mut h, mut rx) = strip_harness(SYNC_SLICE as usize / 2 + 1);
        let total = row_count(&strip_doc(&h)) as u32;

        let first = strip_frames(&frame(&mut h, &mut rx)).0;
        let pending = h.stoat.minimap_build_pending;
        let second = strip_frames(&frame(&mut h, &mut rx)).0;
        let spans = |splices: &[Splice]| {
            splices
                .iter()
                .map(|(_, start, removed, lines)| (*start, *removed, lines.len() as u32))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (
                spans(&first),
                pending,
                spans(&second),
                h.stoat.minimap_build_pending
            ),
            (
                vec![(0, 0, SYNC_SLICE)],
                true,
                vec![(SYNC_SLICE, 0, total - SYNC_SLICE)],
                false,
            ),
        );
    }

    /// The strip goes when the workspace's last overview closes.
    #[test]
    fn closing_the_last_overview_drops_the_strip() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let content_id = strip_id(&h);

        dispatch(&mut h.stoat, &stoat_action::CloseOverview);
        let drops: Vec<u32> = frame(&mut h, &mut rx)
            .iter()
            .filter_map(|command| match command {
                Command::MinimapDrop(drop) => Some(drop.content_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            (
                drops,
                h.stoat
                    .overview_strips
                    .contains_key(&h.stoat.active_workspace)
            ),
            (vec![content_id], false),
        );
    }

    /// A theme change stales the overview document, so the rebuild bakes the
    /// new theme's colors for the strip to read.
    #[test]
    fn a_theme_change_stales_the_overview_document() {
        let (mut h, _rx) = strip_harness(2);
        let fresh = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            ws.overview_doc
                .as_ref()
                .is_some_and(|built| built.stamp == OverviewStamp::read(ws))
        };
        let before = fresh(&h);
        dispatch(
            &mut h.stoat,
            &stoat_action::SetTheme {
                name: "default_dark".into(),
            },
        );
        assert_eq!((before, fresh(&h)), (true, false));
    }

    /// In per-pane mode an overview places its thumb on its own strip, where
    /// the paint reserved one.
    #[test]
    fn a_per_pane_overview_places_its_thumb_on_its_own_strip() {
        let (mut h, mut rx) = strip_harness(8);
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.resize(120, 10);
        let (_, views) = strip_frames(&frame(&mut h, &mut rx));
        let ws = h.stoat.active_workspace();
        let index = ws.panes.pane(ws.panes.focus()).index;
        let (_, rows) = view_of(&h);
        assert_eq!(views, [(index, 0, rows as u16)]);
    }

    /// In single mode the band follows focus, so an overview that a dock took
    /// focus from places no thumb.
    #[test]
    fn an_unfocused_overview_places_no_thumb_on_the_band() {
        let (mut h, mut rx) = strip_harness(8);
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        let (_, views) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(views, []);
    }

    /// A terminal whose protocol stops reading population blocks gets every
    /// line again, without them.
    #[test]
    fn a_protocol_change_sends_the_lines_again() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let doc = strip_doc(&h);

        h.stoat.stoatty_protocol = 12;
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        let held = row_count(&doc) as u32;
        assert_eq!(
            splices,
            [(strip_id(&h), 0, held, wire_lines(&h, &doc, false))]
        );
    }

    /// A document replaced before its fill ends removes only the lines the
    /// terminal holds, and the slices after its first remove none.
    #[test]
    fn a_replacement_mid_fill_removes_the_lines_sent() {
        let (mut h, mut rx) = strip_harness(SYNC_SLICE as usize / 2 + 1);
        frame(&mut h, &mut rx);

        edit_and_rebuild(&mut h, 0..0, "x\n");
        let spans: Vec<(u32, u32)> = (0..2)
            .flat_map(|_| strip_frames(&frame(&mut h, &mut rx)).0)
            .map(|(_, start, removed, _)| (start, removed))
            .collect();
        assert_eq!(spans, [(0, SYNC_SLICE), (SYNC_SLICE, 0)]);
    }

    /// A document with no rows empties the strip in one splice.
    #[test]
    fn a_document_with_no_rows_empties_the_strip() {
        let (mut h, mut rx) = strip_harness(1);
        frame(&mut h, &mut rx);
        let held = row_count(&strip_doc(&h)) as u32;

        edit_and_rebuild(&mut h, 1..2, "");
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(splices, [(strip_id(&h), 0, held, vec![])]);
    }
}
