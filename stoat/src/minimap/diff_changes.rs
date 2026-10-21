//! Minimap lines for the changes view, one per stack row.
//!
//! The changes view strip holds a line for each row the changes view's stack
//! lays out, with a header bar on each excerpt's header row, so the thumb maps
//! the stack's scroll straight onto strip lines.
//!
//! A workspace holds one strip for its changes views. It follows the layout of
//! the focused changes view, else the first one in pane order, and fills a
//! slice per frame, as a freshly opened file fills its own strip.

use super::{
    emit::{convert_minimap_runs, POPULATION_PALETTE_PROTOCOL, WEIGHTED_RUN_PROTOCOL},
    ClassTable, EdgeClass, LineRuns, Run, MAX_COLUMNS, SYNC_SLICE,
};
use crate::{
    app::Stoat,
    diff_changes::{self, DiffChangesId},
    pane::View,
    smooth_scroll::non_pane_pool,
    workspace::{Workspace, WorkspaceId},
};
use std::{mem, ops::Range};
use stoat_widgets::pool::MinimapWindowInputs;
use stoatty_protocol::command::{self, MinimapLinesCommand};

/// The minimap strip of a workspace's changes views, kept in step with the
/// stack layout it follows.
///
/// The terminal holds the lines already sent. A new layout replaces them, and
/// the first splice of the new lines removes every line the terminal still
/// holds.
pub(crate) struct DiffChangesStrip {
    content_id: u32,
    /// The stack rows of the excerpt headers and the stack's total rows, as
    /// the layout the strip's lines show had them.
    synced: Option<(Vec<usize>, usize)>,
    /// Whether the lines carry population blocks, which the terminal's protocol
    /// decides.
    populations: bool,
    /// Rows of the synced layout.
    total: usize,
    /// Rows of the synced layout already sent.
    built_upto: usize,
    /// Lines of a replaced layout that the next splice removes.
    pending_removed: usize,
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

    /// Whether rows of the synced layout are left to send.
    pub(crate) fn build_pending(&self) -> bool {
        self.built_upto < self.total
    }
}

/// The minimap lines of the stack rows in `rows`, a header bar on each row
/// that `headers` names and an empty line on every other row.
///
/// A header bar crosses the whole strip in the header class, so each file of
/// the stack reads as one mark. `headers` runs in ascending order, as
/// [`diff_changes::layout_stack`] writes the rows.
pub(crate) fn stack_lines(
    headers: &[usize],
    rows: Range<usize>,
    table: &ClassTable,
) -> Vec<LineRuns> {
    rows.map(|row| match headers.binary_search(&row) {
        Ok(_) => header_bar(table),
        Err(_) => LineRuns::default(),
    })
    .collect()
}

/// Bring the changes view strip of workspace `ws_id` in step with the stack
/// layout it follows, and append at most one slice of lines to `out`.
///
/// A new layout replaces the strip's lines, and so does a protocol that
/// changes whether lines carry population blocks. Each call sends the next
/// slice, so a long stack fills over idle frames. A stack with no rows empties
/// the strip in one splice.
pub(crate) fn sync_diff_changes_strip(stoat: &mut Stoat, ws_id: WorkspaceId, out: &mut Vec<u8>) {
    let populations = stoat.stoatty_protocol >= POPULATION_PALETTE_PROTOCOL;
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
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

    let layout = strip_layout(ws);
    if layout != strip.synced || strip.populations != populations {
        // The terminal holds the lines already sent, and the lines an earlier
        // replacement has yet to remove when it sent none.
        strip.pending_removed += strip.built_upto;
        strip.total = layout.as_ref().map_or(0, |(_, total)| *total);
        strip.built_upto = 0;
        strip.synced = layout;
        strip.populations = populations;
    }
    if strip.built_upto == strip.total && strip.pending_removed == 0 {
        return;
    }

    let rows = strip.built_upto..(strip.built_upto + SYNC_SLICE as usize).min(strip.total);
    let headers = strip
        .synced
        .as_ref()
        .map_or(&[][..], |(headers, _)| headers);
    let splice = MinimapLinesCommand {
        content_id: strip.content_id,
        start: rows.start as u32,
        removed: mem::take(&mut strip.pending_removed) as u32,
        lines: stack_lines(headers, rows.clone(), minimap_class_table)
            .into_iter()
            .map(convert_minimap_runs)
            .collect(),
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

/// The excerpt header rows and the total rows of the stack that `ws`'s strip
/// follows, which is the focused changes view's, else the first one in pane
/// order.
fn strip_layout(ws: &mut Workspace) -> Option<(Vec<usize>, usize)> {
    let id = ws.focused_diff_changes_id().or_else(|| {
        ws.panes
            .split_panes()
            .find_map(|(_, pane)| match pane.view {
                View::DiffChanges(id) => Some(id),
                _ => None,
            })
    })?;
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let state = diff_changes_views.get_mut(id)?;
    let total = diff_changes::layout_stack(state, editors);
    let headers = state.excerpts.iter().map(|excerpt| excerpt.top).collect();
    Some((headers, total))
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

#[cfg(test)]
mod tests {
    use super::{stack_lines, strip_layout};
    use crate::{
        action_handlers::dispatch,
        apc_emit,
        minimap::{
            emit::{self, convert_minimap_runs},
            ClassTable, EdgeClass, Run, MAX_COLUMNS, SYNC_SLICE,
        },
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::SINGLE_MINIMAP_STRIP_ID,
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::Theme,
    };
    use std::{
        ops::Range,
        path::{Path, PathBuf},
    };
    use stoat_config::MinimapMode;
    use stoatty_protocol::command::{Command, LineSummary};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A header row takes a bar across the strip in the header class, and every
    /// other row an empty line, over the rows asked for alone.
    #[test]
    fn a_header_row_takes_a_bar_and_every_other_row_none() {
        let table = ClassTable::from_theme(&Theme::empty());
        let bar = vec![Run {
            start_col: 0,
            len: MAX_COLUMNS as u8,
            class: table.edge_class(EdgeClass::Header),
            weight: 255,
        }];
        let lines: Vec<Vec<Run>> = stack_lines(&[0, 3], 1..5, &table)
            .iter()
            .map(|line| line.as_slice().to_vec())
            .collect();
        assert_eq!(lines, [vec![], vec![], bar, vec![]]);
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

    /// The header rows and the total rows of the stack the strip follows.
    fn layout(h: &mut TestHarness) -> (Vec<usize>, usize) {
        strip_layout(h.stoat.active_workspace_mut()).expect("a changes view to follow")
    }

    /// Every line of the stack's layout in the wire form the strip sends.
    fn wire_lines(h: &mut TestHarness, rows: Range<usize>) -> Vec<LineSummary> {
        let (headers, _) = layout(h);
        stack_lines(&headers, rows, &h.stoat.minimap_class_table)
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
