//! Minimap lines for the changes view, one per stack row.
//!
//! The changes view strip holds a line for each row the changes view's stack
//! lays out, so the thumb maps the stack's scroll straight onto strip lines.
//! An excerpt's header row takes a header bar. Each display row of an excerpt
//! summarizes the file's text with its syntax colors and the mark of its
//! change, or the base line that the diff view splices in for a removal.
//!
//! A workspace holds one strip for its changes views. It follows the stack of
//! the focused changes view, else the first one in pane order, and fills a
//! slice per frame, as a freshly opened file fills its own strip.

use super::{
    emit::{
        convert_minimap_runs, minimap_line_tokens, POPULATION_PALETTE_PROTOCOL,
        WEIGHTED_RUN_PROTOCOL,
    },
    summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run, MAX_COLUMNS,
    SYNC_SLICE,
};
use crate::{
    app::Stoat,
    diff_changes::{self, DiffChangesId, DiffChangesState, StackStamp},
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::{EditorId, EditorState},
    host::DiffStatus,
    pane::View,
    smooth_scroll::non_pane_pool,
    workspace::{Workspace, WorkspaceId},
};
use slotmap::SlotMap;
use std::{collections::HashMap, mem, ops::Range};
use stoat_widgets::pool::MinimapWindowInputs;
use stoatty_protocol::command::{self, MinimapLinesCommand};

/// The minimap strip of a workspace's changes views, kept in step with the
/// stack it follows.
///
/// The terminal holds the lines already sent. A stack whose key moves replaces
/// them, and the first splice of the new lines removes every line the terminal
/// still holds.
pub(crate) struct DiffChangesStrip {
    content_id: u32,
    /// The key of the stack the strip's lines summarize, or fill from.
    synced: Option<StripKey>,
    /// Whether the lines carry population blocks, which the terminal's protocol
    /// decides.
    populations: bool,
    /// Rows of the synced stack.
    total: usize,
    /// Rows of the synced stack already sent.
    built_upto: usize,
    /// Lines of a replaced stack that the next splice removes.
    pending_removed: usize,
}

/// What the lines of a changes view strip show, as of the stack layout and the
/// versions they were summarized at.
///
/// A move in any part changes the line of some row, so a key that differs
/// sends the strip's lines again.
#[derive(Clone, PartialEq, Eq)]
struct StripKey {
    /// The workspace stamp the stack's list was made at.
    stamp: Option<StackStamp>,
    /// Each excerpt's part, in stack order.
    excerpts: Vec<ExcerptKey>,
    /// Rows of the stack.
    total: usize,
    /// Whether the rows take syntax colors at all.
    syntax_on: bool,
}

/// One excerpt's part of a [`StripKey`].
#[derive(Clone, PartialEq, Eq)]
struct ExcerptKey {
    /// The excerpt's header row.
    top: usize,
    /// The diff map version the excerpt's folds were cut from.
    folded_version: Option<usize>,
    /// The parse that colors the excerpt's rows.
    ///
    /// A parse lands with no fold of its own, so the colors need a version of
    /// their own.
    syntax_version: Option<u64>,
    /// The LSP semantic tokens that color the excerpt's rows.
    lsp_token_version: Option<u64>,
}

impl DiffChangesStrip {
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

    /// Whether rows of the synced stack are left to send.
    pub(crate) fn build_pending(&self) -> bool {
        self.built_upto < self.total
    }
}

/// The minimap lines of the stack rows in `rows`, as the changes view paints
/// those rows, as of the last [`diff_changes::layout_stack`].
///
/// An excerpt's header row is a bar across the whole strip in the header class,
/// so each file of the stack reads as one mark. A display row that shows the
/// file's text summarizes that text with its syntax tokens and the mark of its
/// diff status. A base line spliced in for a removal summarizes its text with
/// the deleted mark, and a fold's placeholder row summarizes the placeholder
/// with no mark.
///
/// With `populations` unset, every content class stays in the plain syntax
/// block, for a terminal that reads no population blocks. With `syntax_on`
/// unset, no row takes syntax tokens. A range past the stack's last row yields
/// lines up to that row only.
pub(crate) fn summarize_rows(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    rows: Range<usize>,
    table: &ClassTable,
    populations: bool,
    syntax_on: bool,
) -> Vec<LineRuns> {
    let mut lines = Vec::with_capacity(rows.len());
    for excerpt in &state.excerpts {
        let first = excerpt.top + 1;
        let end = (first + diff_changes::excerpt_rows(excerpt, editors)).min(rows.end);
        if excerpt.top >= rows.start && excerpt.top < rows.end {
            lines.push(header_bar(table));
        }
        let display_rows = rows.start.max(first) - first..end.saturating_sub(first);
        if display_rows.is_empty() {
            continue;
        }
        let Some(editor) = editors.get_mut(excerpt.editor_id) else {
            continue;
        };

        let kept = diff_changes::kept_rows(editor);
        let snapshot = editor.display_map.snapshot();
        let display_rows = display_rows.start as u32..display_rows.end as u32;
        let buffer_row = |row: u32| {
            snapshot
                .display_to_buffer(DisplayPoint::new(row, 0))
                .map(|point| point.row)
        };
        let buffer_rows = match (
            display_rows.clone().find_map(buffer_row),
            display_rows.clone().rev().find_map(buffer_row),
        ) {
            (Some(start), Some(last)) => start..last + 1,
            _ => 0..0,
        };
        let tokens =
            minimap_line_tokens(&snapshot, excerpt.buffer_id, syntax_on, table, buffer_rows);
        lines.extend(
            display_rows
                .map(|row| excerpt_row_line(&snapshot, row, &kept, &tokens, table, populations)),
        );
    }
    lines
}

/// Bring the changes view strip of workspace `ws_id` in step with the stack it
/// follows, and append at most one slice of lines to `out`.
///
/// A stack whose key moves replaces the strip's lines, and so does a protocol
/// that changes whether lines carry population blocks. Each call sends the
/// next slice, so a long stack fills over idle frames. A stack with no rows
/// empties the strip in one splice.
pub(crate) fn sync_diff_changes_strip(stoat: &mut Stoat, ws_id: WorkspaceId, out: &mut Vec<u8>) {
    let populations = stoat.stoatty_protocol >= POPULATION_PALETTE_PROTOCOL;
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
    let syntax_on = stoat.syntax_highlight;
    let Stoat {
        workspaces,
        diff_changes_strips,
        minimap_class_table,
        ..
    } = stoat;
    let (Some(ws), Some(strip)) = (
        workspaces.get_mut(ws_id),
        diff_changes_strips.get_mut(&ws_id),
    ) else {
        return;
    };

    let followed = followed_view(ws);
    let key = followed.and_then(|id| strip_key(ws, id, syntax_on));
    if key != strip.synced || strip.populations != populations {
        // The terminal holds the lines already sent, and the lines an earlier
        // replacement has yet to remove when it sent none.
        strip.pending_removed += strip.built_upto;
        strip.total = key.as_ref().map_or(0, |key| key.total);
        strip.built_upto = 0;
        strip.synced = key;
        strip.populations = populations;
    }
    if strip.built_upto == strip.total && strip.pending_removed == 0 {
        return;
    }

    let rows = strip.built_upto..(strip.built_upto + SYNC_SLICE as usize).min(strip.total);
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let lines = followed
        .and_then(|id| diff_changes_views.get(id))
        .map_or_else(Vec::new, |state| {
            summarize_rows(
                state,
                editors,
                rows.clone(),
                minimap_class_table,
                populations,
                syntax_on,
            )
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

/// Place the thumb of changes view `id` in workspace `ws_id` on strip
/// `strip_id`, over the rows that the changes view's last paint showed.
///
/// The thumb passes the change gate the editor thumbs pass, under a pool id of
/// its own. A strip that shows the changes view's thumb and then an editor's
/// therefore gets the editor's thumb back, though the editor's view never
/// moved.
pub(crate) fn emit_diff_changes_thumb(
    stoat: &mut Stoat,
    ws_id: WorkspaceId,
    id: DiffChangesId,
    strip_id: u32,
    out: &mut Vec<u8>,
) {
    let Some(state) = stoat
        .workspaces
        .get(ws_id)
        .and_then(|ws| ws.diff_changes_views.get(id))
    else {
        return;
    };
    let top = state.scroll as f32;
    let rows = state.viewport_rows.max(1) as u32;
    let visible = rows.min(u16::MAX as u32) as u16;
    stoat.smooth_scroll.emit_minimap_view(
        out,
        strip_id,
        non_pane_pool::DIFF_CHANGES_THUMB,
        MinimapWindowInputs::new(top, 0, rows),
        || (top, visible),
    );
}

/// The changes view whose stack `ws`'s strip follows, which is the focused
/// one, else the first one in pane order.
fn followed_view(ws: &Workspace) -> Option<DiffChangesId> {
    ws.focused_diff_changes_id().or_else(|| {
        ws.panes
            .split_panes()
            .find_map(|(_, pane)| match pane.view {
                View::DiffChanges(id) => Some(id),
                _ => None,
            })
    })
}

/// The key of the lines that changes view `id` of `ws` gives its strip, after a
/// fresh layout of its stack.
fn strip_key(ws: &mut Workspace, id: DiffChangesId, syntax_on: bool) -> Option<StripKey> {
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let state = diff_changes_views.get_mut(id)?;
    let total = diff_changes::layout_stack(state, editors);
    let excerpts = state
        .excerpts
        .iter()
        .map(|excerpt| ExcerptKey {
            top: excerpt.top,
            folded_version: excerpt.folded_version,
            syntax_version: buffers.syntax_version(excerpt.buffer_id),
            lsp_token_version: buffers.lsp_token_version(excerpt.buffer_id),
        })
        .collect();
    Some(StripKey {
        stamp: state.synced,
        excerpts,
        total,
        syntax_on,
    })
}

/// An excerpt's header row, a bar across the whole strip in the header class.
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

/// The minimap line of display row `row` of an excerpt whose display is
/// `snapshot`, with the buffer rows `kept` showing and `tokens` coloring them.
///
/// A row that shows the file's text summarizes the buffer bytes the row covers
/// rather than its display text. The display text expands tabs while the
/// tokens count buffer bytes, so on a tab-indented line the display text puts
/// each color at the wrong column. A row whose buffer row no kept run holds is
/// a fold's placeholder, which takes no tokens, since its buffer row is hidden.
fn excerpt_row_line(
    snapshot: &DisplaySnapshot,
    row: u32,
    kept: &[Range<u32>],
    tokens: &HashMap<u32, Vec<LineToken>>,
    table: &ClassTable,
    populations: bool,
) -> LineRuns {
    let Some(start) = snapshot.display_to_buffer(DisplayPoint::new(row, 0)) else {
        return summarize_line(
            &snapshot.display_line(row),
            &[],
            deleted_mark(table, populations),
        );
    };
    if !kept.iter().any(|run| run.contains(&start.row)) {
        return summarize_line(&snapshot.display_line(row), &[], RowMark::NONE);
    }

    let mut line = String::new();
    snapshot
        .buffer_snapshot()
        .rope()
        .line_walk(start.row..start.row + 1)
        .next_into(&mut line);
    // A soft-wrapped line continues on the next display row, so this row ends
    // where that one starts.
    let end = match snapshot.display_to_buffer(DisplayPoint::new(row + 1, 0)) {
        Some(next) if next.row == start.row && next.column > start.column => next.column as usize,
        _ => line.len(),
    };
    let from = (start.column as usize).min(line.len());
    let end = end.clamp(from, line.len());
    let row_tokens: Vec<LineToken> = tokens.get(&start.row).map_or_else(Vec::new, |tokens| {
        tokens
            .iter()
            .filter_map(|token| {
                let range = token.range.start.max(from)..token.range.end.min(end);
                (range.start < range.end).then(|| LineToken {
                    range: range.start - from..range.end - from,
                    class: token.class,
                })
            })
            .collect()
    });
    let mark = status_mark(snapshot.line_diff_status(start.row), table, populations);
    summarize_line(&line[from..end], &row_tokens, mark)
}

/// The mark of a row of diff `status`.
///
/// An addition takes the added lane and a modification or a move the modified
/// lane, each with its own population block when `populations` holds. An
/// unchanged row takes no mark.
fn status_mark(status: DiffStatus, table: &ClassTable, populations: bool) -> RowMark {
    let edge = match status {
        DiffStatus::Added => EdgeClass::Added,
        DiffStatus::Modified | DiffStatus::Moved => EdgeClass::Modified,
        DiffStatus::Unchanged => return RowMark::NONE,
    };
    RowMark {
        edge: Some(table.edge_class(edge)),
        class_base: match populations {
            true => table.population_base(status),
            false => 0,
        },
    }
}

/// The mark of a base line spliced in for a removal, the removed lane with the
/// deleted block when `populations` holds.
fn deleted_mark(table: &ClassTable, populations: bool) -> RowMark {
    RowMark {
        edge: Some(table.edge_class(EdgeClass::Removed)),
        class_base: match populations {
            true => table.deleted_base(),
            false => 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{status_mark, summarize_rows};
    use crate::{
        action_handlers::dispatch,
        apc_emit,
        app::Stoat,
        buffer_registry::RetainedLspTokens,
        diff_changes,
        display_map::highlights::{BufferSemanticTokens, SemanticTokenHighlight},
        host::DiffStatus,
        minimap::{
            emit::{self, convert_minimap_runs, POPULATION_PALETTE_PROTOCOL},
            summarize_line, ClassTable, EdgeClass, LineRuns, LineToken, RowMark, Run, MAX_COLUMNS,
            SYNC_SLICE,
        },
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::SINGLE_MINIMAP_STRIP_ID,
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::Theme,
        workspace::Workspace,
    };
    use std::{
        ops::Range,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_config::MinimapMode;
    use stoat_language::HighlightId;
    use stoat_text::Bias;
    use stoatty_protocol::command::{Command, LineSummary};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// `files` staged as changes under a focused changes view, painted once at
    /// a width that pairs each modified line with its base line.
    fn files_of(files: &[(&str, &str, &str)]) -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stage_review_scenario("/repo", files);
        h.cover_with_diff_changes(false);
        h.settle();
        let _ = h.stoat.render();
        h
    }

    /// One file whose excerpt shows a context row, a modified row, a context
    /// row, a deleted base row, and the empty row after the last newline.
    fn one_file() -> TestHarness {
        files_of(&[("a.txt", "a\nb\nc\nd\n", "a\nB\nc\n")])
    }

    /// The lines `summarize_rows` gives the focused stack's `rows`, with
    /// `table`, or the harness's own class table for `None`.
    fn stack_lines(
        h: &mut TestHarness,
        rows: Range<usize>,
        table: Option<&ClassTable>,
        populations: bool,
    ) -> Vec<LineRuns> {
        let id = h
            .stoat
            .active_workspace()
            .focused_diff_changes_id()
            .expect("a focused changes view");
        let (ws_id, syntax_on) = (h.stoat.active_workspace, h.stoat.syntax_highlight);
        let Stoat {
            workspaces,
            minimap_class_table,
            ..
        } = &mut h.stoat;
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = &mut workspaces[ws_id];
        let state = &mut diff_changes_views[id];
        diff_changes::layout_stack(state, editors);
        let table = table.unwrap_or(minimap_class_table);
        summarize_rows(state, editors, rows, table, populations, syntax_on)
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
        let mut h = one_file();
        let expected = |populations: bool| {
            let mark = |edge, base| RowMark {
                edge: Some(table.edge_class(edge)),
                class_base: if populations { base } else { 0 },
            };
            let line = |text, mark| summarize_line(text, &[], mark).as_slice().to_vec();
            vec![
                vec![Run {
                    start_col: 0,
                    len: MAX_COLUMNS as u8,
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
                line("", RowMark::NONE),
            ]
        };

        let (_, total) = layout(&mut h);
        assert_eq!(
            [true, false].map(|populations| runs(&stack_lines(
                &mut h,
                0..total,
                Some(&table),
                populations
            ))),
            [expected(true), expected(false)],
        );
    }

    /// Any range yields the lines the whole walk gives its rows, a range that
    /// starts past the first excerpt included, and a range past the last row
    /// stops at it.
    #[test]
    fn a_range_yields_the_lines_of_its_rows() {
        let table = ClassTable::from_theme(&Theme::empty());
        let mut h = files_of(&[
            ("a.txt", "a\nb\nc\nd\n", "a\nB\nc\n"),
            ("b.txt", "x\n", "y\n"),
        ]);
        let (headers, count) = layout(&mut h);
        let first = headers[1];
        let whole = runs(&stack_lines(&mut h, 0..count, Some(&table), true));

        let ranges = [first..count, 1..3, 2..first + 2, count - 1..count + 5];
        assert_eq!(
            ranges
                .clone()
                .map(|rows| runs(&stack_lines(&mut h, rows, Some(&table), true))),
            ranges.map(|rows| whole[rows.start..rows.end.min(count)].to_vec()),
        );
    }

    /// Put a token of the first syntax scope over `bytes` of the focused
    /// excerpt's buffer, and answer the class it resolves to.
    fn token_over(h: &mut TestHarness, bytes: Range<usize>) -> u8 {
        let style = h
            .stoat
            .syntax_styles
            .id_for_highlight(HighlightId(0))
            .expect("the first theme key resolves");
        let interner = h.stoat.syntax_styles.interner.clone();
        let (editor_id, buffer_id) = h.stoat.focused_editor_ids().expect("an excerpt");
        let spans: Arc<[SemanticTokenHighlight]> = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            Arc::from(vec![SemanticTokenHighlight {
                range: snap.anchor_at(bytes.start, Bias::Right)
                    ..snap.anchor_at(bytes.end, Bias::Left),
                style,
            }])
        };
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, spans, interner);
        h.stoat.minimap_class_table.class_of(style)
    }

    /// A row reads the tokens of its own buffer line, each resolved to its
    /// class, and a base row spliced in for a removal takes none.
    #[test]
    fn a_row_reads_the_tokens_of_its_own_line() {
        let mut h = one_file();
        // B is byte 2 of "a\nB\nc\n".
        let class = token_over(&mut h, 2..3);
        let (_, total) = layout(&mut h);
        let lines = runs(&stack_lines(&mut h, 0..total, None, true));

        let table = &h.stoat.minimap_class_table;
        let modified = RowMark {
            edge: Some(table.edge_class(EdgeClass::Modified)),
            class_base: table.population_base(DiffStatus::Modified),
        };
        let deleted = RowMark {
            edge: Some(table.edge_class(EdgeClass::Removed)),
            class_base: table.deleted_base(),
        };
        let token = [LineToken { range: 0..1, class }];
        assert_eq!(
            (class != 0, [lines[2].clone(), lines[4].clone()]),
            (
                true,
                [
                    summarize_line("B", &token, modified).as_slice().to_vec(),
                    summarize_line("d", &[], deleted).as_slice().to_vec(),
                ]
            ),
        );
    }

    /// A tab-indented row takes its colors at the columns the tab leaves, the
    /// summary the editor's own strip gives that buffer line.
    #[test]
    fn a_tab_indented_row_takes_its_colors_past_the_tab() {
        let mut h = files_of(&[("a.txt", "\tx\n", "\ty\n")]);
        let class = token_over(&mut h, 1..2);
        let lines = runs(&stack_lines(&mut h, 1..2, None, true));

        let table = &h.stoat.minimap_class_table;
        let modified = RowMark {
            edge: Some(table.edge_class(EdgeClass::Modified)),
            class_base: table.population_base(DiffStatus::Modified),
        };
        let token = [LineToken { range: 1..2, class }];
        assert_eq!(
            lines,
            [summarize_line("\ty", &token, modified).as_slice().to_vec()]
        );
    }

    /// A fold's placeholder row summarizes the placeholder with no mark and no
    /// tokens, though the hidden row under it holds a token.
    #[test]
    fn a_fold_placeholder_row_takes_no_tokens() {
        let base: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let mut h = files_of(&[("a.txt", &base, &base.replace("\n15\n", "\nFIFTEEN\n"))]);
        token_over(&mut h, 0..1);
        let placeholder = {
            let ws = h.stoat.active_workspace_mut();
            let (editor_id, _) = ws.focused_editor().expect("an excerpt");
            ws.editors[editor_id].display_map.snapshot().display_line(0)
        };

        let lines = runs(&stack_lines(&mut h, 1..2, None, true));
        assert_eq!(
            lines,
            [summarize_line(&placeholder, &[], RowMark::NONE)
                .as_slice()
                .to_vec()]
        );
    }

    /// An addition takes the added lane and a modification or a move the
    /// modified lane, each with its own population block, and an unchanged
    /// row takes no mark. Without populations every block is plain.
    #[test]
    fn each_diff_status_takes_its_lane_and_block() {
        let table = ClassTable::from_theme(&Theme::empty());
        let mark = |edge, status, populations| RowMark {
            edge: Some(table.edge_class(edge)),
            class_base: match populations {
                true => table.population_base(status),
                false => 0,
            },
        };
        let statuses = [
            DiffStatus::Added,
            DiffStatus::Moved,
            DiffStatus::Modified,
            DiffStatus::Unchanged,
        ];
        assert_eq!(
            [true, false].map(|populations| {
                statuses.map(|status| status_mark(status, &table, populations))
            }),
            [true, false].map(|populations| {
                [
                    mark(EdgeClass::Added, DiffStatus::Added, populations),
                    mark(EdgeClass::Modified, DiffStatus::Moved, populations),
                    mark(EdgeClass::Modified, DiffStatus::Modified, populations),
                    RowMark::NONE,
                ]
            }),
        );
    }

    /// `files` staged as changes under a focused changes view, on a stoatty of
    /// protocol 13, in a window short enough that the changes view scrolls.
    ///
    /// The changes view covers the first file's editor with no landing, so no
    /// held screen row scrolls the first frame off the top of the stack.
    fn strip_harness_of(
        files: &[(String, String, String)],
    ) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = TestHarness::with_size(100, 10);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        let files: Vec<(&str, &str, &str)> = files
            .iter()
            .map(|(path, base, buffer)| (path.as_str(), base.as_str(), buffer.as_str()))
            .collect();
        h.stage_review_scenario("/repo", &files);
        h.open_file(Path::new(&format!("/repo/{}", files[0].0)));
        h.cover_with_diff_changes(false);
        h.settle();
        (h, rx)
    }

    /// `count` changed one-line files under a focused changes view.
    fn strip_harness(count: usize) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let files: Vec<(String, String, String)> = (0..count)
            .map(|i| (format!("f{i:03}.txt"), format!("{i}\n"), format!("{i}!\n")))
            .collect();
        strip_harness_of(&files)
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

    fn strip_id(h: &TestHarness) -> u32 {
        h.stoat.diff_changes_strips[&h.stoat.active_workspace].content_id()
    }

    /// The changes view's scroll and its painted rows.
    fn view_of(h: &TestHarness) -> (usize, usize) {
        let state = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .expect("a focused changes view");
        (state.scroll, state.viewport_rows)
    }

    /// Each excerpt's header row and the total rows of the focused stack.
    fn layout(h: &mut TestHarness) -> (Vec<usize>, usize) {
        let ws = h.stoat.active_workspace_mut();
        let id = ws
            .focused_diff_changes_id()
            .expect("a focused changes view");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        let state = &mut diff_changes_views[id];
        let total = diff_changes::layout_stack(state, editors);
        (
            state.excerpts.iter().map(|excerpt| excerpt.top).collect(),
            total,
        )
    }

    /// The stack's `rows` in the wire form the strip sends, with the population
    /// blocks the terminal's protocol reads.
    fn wire_lines(h: &mut TestHarness, rows: Range<usize>) -> Vec<LineSummary> {
        let populations = h.stoat.stoatty_protocol >= POPULATION_PALETTE_PROTOCOL;
        stack_lines(h, rows, None, populations)
            .into_iter()
            .map(convert_minimap_runs)
            .collect()
    }

    /// Replace `range` in the open buffer of `file` with `text`, then let the
    /// changes view refold over the diff map that follows.
    fn edit_and_refold(h: &mut TestHarness, file: &str, range: Range<usize>, text: &str) {
        let ws = h.stoat.active_workspace();
        let buffer_id = ws
            .buffers
            .id_for_path(Path::new(file))
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

    /// The first frame fills the strip with every stack row in one splice, and
    /// places the thumb over the painted rows on the band.
    #[test]
    fn the_first_frame_fills_the_strip_and_places_its_thumb() {
        let (mut h, mut rx) = strip_harness(8);
        let frames = strip_frames(&frame(&mut h, &mut rx));
        let (_, rows) = view_of(&h);
        let (_, total) = layout(&mut h);
        assert_eq!(
            frames,
            (
                vec![(strip_id(&h), 0, 0, wire_lines(&mut h, 0..total))],
                vec![(SINGLE_MINIMAP_STRIP_ID, 0, rows as u16)],
            ),
        );
    }

    /// A scroll moves the thumb alone, and a dial step sends neither lines nor
    /// a thumb.
    #[test]
    fn a_scroll_moves_the_thumb_and_a_dial_step_sends_nothing() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);

        dispatch(&mut h.stoat, &stoat_action::DiffChangesLast);
        let scrolled = strip_frames(&frame(&mut h, &mut rx));
        let (top, rows) = view_of(&h);
        dispatch(&mut h.stoat, &stoat_action::DiffTintUp);
        let dialed = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            (top > 0, scrolled, dialed),
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

    /// A layout that moves replaces every line the strip holds, from its first
    /// row.
    #[test]
    fn a_moved_layout_replaces_the_strips_lines() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);

        edit_and_refold(&mut h, "/repo/f000.txt", 0..0, "x\n");
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        let (_, total) = layout(&mut h);
        assert_eq!(
            (total > held, splices),
            (
                true,
                vec![(strip_id(&h), 0, held as u32, wire_lines(&mut h, 0..total))]
            ),
        );
    }

    /// A stack past one slice fills over several frames, and the frames
    /// between keep the build pending.
    #[test]
    fn a_long_stack_fills_a_slice_per_frame() {
        let lines = SYNC_SLICE as usize + 40;
        let base: String = (0..lines).map(|n| format!("{n}\n")).collect();
        let buffer: String = (0..lines).map(|n| format!("{n}!\n")).collect();
        let (mut h, mut rx) = strip_harness_of(&[("long.txt".to_string(), base, buffer)]);

        // The paint fits each excerpt to the pane, which settles its rows.
        let first = strip_frames(&frame(&mut h, &mut rx)).0;
        let (_, total) = layout(&mut h);
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
                vec![(SYNC_SLICE, 0, total as u32 - SYNC_SLICE)],
                false,
            ),
        );
    }

    /// The strip goes when the workspace's last changes view closes.
    #[test]
    fn closing_the_last_changes_view_drops_the_strip() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let content_id = strip_id(&h);

        dispatch(&mut h.stoat, &stoat_action::CloseDiffChanges);
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
                    .diff_changes_strips
                    .contains_key(&h.stoat.active_workspace)
            ),
            (vec![content_id], false),
        );
    }

    /// In per-pane mode a changes view places its thumb on its own strip, where
    /// the paint reserved one.
    #[test]
    fn a_per_pane_changes_view_places_its_thumb_on_its_own_strip() {
        let (mut h, mut rx) = strip_harness(8);
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.resize(120, 10);
        let (_, views) = strip_frames(&frame(&mut h, &mut rx));
        let ws = h.stoat.active_workspace();
        let index = ws.panes.pane(ws.panes.focus()).index;
        let (_, rows) = view_of(&h);
        assert_eq!(views, [(index, 0, rows as u16)]);
    }

    /// In single mode the band follows focus, so a changes view that a dock
    /// took focus from places no thumb.
    #[test]
    fn an_unfocused_changes_view_places_no_thumb_on_the_band() {
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
    /// line again.
    #[test]
    fn a_protocol_change_sends_the_lines_again() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);

        h.stoat.stoatty_protocol = 12;
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            splices,
            [(strip_id(&h), 0, held as u32, wire_lines(&mut h, 0..held))]
        );
    }

    /// Turning syntax colors off sends every line again, since it recolors
    /// rows that hold still.
    #[test]
    fn a_syntax_switch_sends_the_lines_again() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);

        h.stoat.syntax_highlight = false;
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            splices,
            [(strip_id(&h), 0, held as u32, wire_lines(&mut h, 0..held))]
        );
    }

    /// A parse that moves with no fold of its own sends every line again,
    /// since the parse colors the rows.
    #[test]
    fn a_moved_parse_sends_the_lines_again() {
        let (mut h, mut rx) = strip_harness_of(&[(
            "a.rs".to_string(),
            "fn a() {}\n".to_string(),
            "fn b() {}\n".to_string(),
        )]);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("an excerpt");
        let parsed = h
            .stoat
            .active_workspace()
            .buffers
            .syntax_version(buffer_id)
            .is_some();

        h.stoat
            .active_workspace_mut()
            .buffers
            .clear_syntax(buffer_id);
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            (parsed, splices),
            (
                true,
                vec![(strip_id(&h), 0, held as u32, wire_lines(&mut h, 0..held))]
            ),
        );
    }

    /// LSP tokens that arrive with no fold of their own send every line again,
    /// since the tokens color the rows.
    #[test]
    fn arrived_lsp_tokens_send_the_lines_again() {
        let (mut h, mut rx) = strip_harness(8);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("an excerpt");

        let interner = h.stoat.syntax_styles.interner.clone();
        let channel = BufferSemanticTokens::new(Arc::from(Vec::new()), interner, |_| 0);
        h.stoat.active_workspace_mut().buffers.store_lsp_tokens(
            buffer_id,
            RetainedLspTokens {
                version: 0,
                channel,
            },
        );
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(
            splices,
            [(strip_id(&h), 0, held as u32, wire_lines(&mut h, 0..held))]
        );
    }

    /// A layout replaced before its fill ends removes only the lines the
    /// terminal holds, and the slices after its first remove none.
    #[test]
    fn a_replacement_mid_fill_removes_the_lines_sent() {
        let lines = SYNC_SLICE as usize + 40;
        let base: String = (0..lines).map(|n| format!("{n}\n")).collect();
        let buffer: String = (0..lines).map(|n| format!("{n}!\n")).collect();
        let (mut h, mut rx) = strip_harness_of(&[("long.txt".to_string(), base, buffer)]);
        frame(&mut h, &mut rx);

        edit_and_refold(&mut h, "/repo/long.txt", 0..0, "x\n");
        let spans: Vec<(u32, u32)> = (0..2)
            .flat_map(|_| strip_frames(&frame(&mut h, &mut rx)).0)
            .map(|(_, start, removed, _)| (start, removed))
            .collect();
        assert_eq!(spans, [(0, SYNC_SLICE), (SYNC_SLICE, 0)]);
    }

    /// A stack whose last file leaves the list empties the strip in one
    /// splice.
    #[test]
    fn a_stack_with_no_excerpt_empties_the_strip() {
        let (mut h, mut rx) = strip_harness(1);
        h.stoat.set_diff_warm_auto(true);
        frame(&mut h, &mut rx);
        let (_, held) = layout(&mut h);

        // A commit of the change empties the tally, and the open buffer's map
        // holds no hunk against the new HEAD, so the file leaves the list.
        h.fake_git()
            .add_repo(PathBuf::from("/repo"))
            .clear_changes()
            .head_file("f000.txt", "0!\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle_diff_jobs();
        let (splices, _) = strip_frames(&frame(&mut h, &mut rx));
        assert_eq!(splices, [(strip_id(&h), 0, held as u32, vec![])]);
    }
}
