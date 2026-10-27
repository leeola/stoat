//! The paint of a [`View::DiffChanges`](crate::pane::View::DiffChanges) pane.
//!
//! The changes view paints its excerpts one under another through the
//! per-file diff's painter, so a hunk reads the same here as in the per-file
//! diff. A header row names each excerpt's file, centered across the pane.
//! The focused excerpt paints the cursor where the changes view's keys act.

use crate::{
    buffer_registry::BufferRegistry,
    diff_changes::{self, DiffChangesState},
    editor_state::{EditorId, EditorState},
    pane::Pane,
    render::{
        editor::{fit_diff_view, render_editor_with_overlay, screen_cell, split_minimap_strip},
        layout::split_pane_status,
        text::{text_width, write_str, write_str_clipped},
        undercurl::UndercurlBatch,
        FrameCtx,
    },
    theme::scope,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use slotmap::SlotMap;
use stoat_widgets::ApcScene;

/// What a paint records on an excerpt's editor about where it laid the
/// excerpt out.
///
/// The terminal cursor, cursor popups, and pointer hits read these back from
/// the live paint, so a page paint puts them back as it found them.
struct PaintLayout {
    scroll_row: u32,
    viewport_rows: Option<u32>,
    cursor_screen_cell: Option<(u16, u16)>,
    text_rect: Option<Rect>,
    minimap_rect: Option<Rect>,
}

impl PaintLayout {
    fn of(editor: &EditorState) -> Self {
        Self {
            scroll_row: editor.scroll_row,
            viewport_rows: editor.viewport_rows,
            cursor_screen_cell: editor.cursor_screen_cell,
            text_rect: editor.text_rect,
            minimap_rect: editor.minimap_rect,
        }
    }

    fn restore(self, editor: &mut EditorState) {
        editor.scroll_row = self.scroll_row;
        editor.viewport_rows = self.viewport_rows;
        editor.cursor_screen_cell = self.cursor_screen_cell;
        editor.text_rect = self.text_rect;
        editor.minimap_rect = self.minimap_rect;
    }
}

/// Paint the stack `state` holds into `pane`'s rows above its status row.
///
/// [`render_pane_status`](crate::render::pane::render_pane_status) paints the
/// status row, into the pane's status lane, so a replay of this paint keeps a
/// fresh row.
///
/// The stack paints from the scroll of `state`, which first moves so the
/// focused excerpt's cursor shows. A screen row that a landing holds takes the
/// cursor's row. Otherwise a cursor that moved since the last paint takes the
/// least scroll that keeps it on screen, and a cursor that stayed leaves the
/// scroll, so a wheel scroll leaves the cursor off screen.
///
/// Each excerpt takes a header row in the commit header style, with its file
/// name centered, and then its rows. The rows paint from the excerpt's own
/// scroll row, so an excerpt cut at the top shows its lower rows. After the
/// paint each excerpt's viewport holds all of its rows, so the editor's own
/// follow never scrolls an excerpt.
///
/// The focused excerpt records the cell of its cursor in
/// [`EditorState::cursor_screen_cell`] when the terminal draws that cursor.
/// The record is `None` when the paint does not lay the excerpt out, such as
/// an excerpt outside the window.
///
/// With no excerpt the pane says "building changes..." while the repository
/// tally is pending, and "no changes" once it is in.
///
/// With per-pane minimaps on, a pane wide enough for the minimap strip gives
/// its right edge to the strip, with or without excerpts. The strip's rect
/// lands on [`DiffChangesState::minimap_rect`], and no excerpt reserves a strip
/// of its own.
///
/// Where [`stack_composited`] holds, the stack's runs and bars go to a scene
/// the paint drops. The pool's pages carry their own, and the composite covers
/// the live ones.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_diff_changes(
    pane: &Pane,
    is_focused: bool,
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (inner, _) = split_pane_status(pane.area);
    let (inner, strip) = split_minimap_strip(inner, frame.minimap_enabled);
    state.minimap_rect = strip;
    if let Some(editor) = state
        .focused_excerpt()
        .and_then(|excerpt| editors.get_mut(excerpt.editor_id))
    {
        editor.cursor_screen_cell = None;
    }
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if state.excerpts.is_empty() {
        let text = match frame.diff_changes_building {
            true => "building changes...",
            false => "no changes",
        };
        write_str(
            buf,
            inner.x,
            inner.y,
            text,
            frame.theme.get(scope::UI_TEXT_MUTED),
        );
        return;
    }

    state.viewport_rows = inner.height as usize;
    for excerpt in &state.excerpts {
        if let Some(editor) = editors.get_mut(excerpt.editor_id) {
            fit_diff_view(editor, inner, frame.wrap_mode, frame.wrap_column);
        }
    }
    let total = diff_changes::layout_stack(state, editors);
    let cursor = diff_changes::focused_cursor(state, editors);
    if let Some(row) = diff_changes::cursor_stack_row(state, editors) {
        match state.held_screen_row.take() {
            Some(screen_row) => {
                let last = state.viewport_rows.saturating_sub(1);
                state.scroll = row.saturating_sub(usize::from(screen_row).min(last));
            },
            None if cursor != state.followed_cursor => {
                state.ensure_row_visible(row, state.viewport_rows);
            },
            None => {},
        }
        state.followed_cursor = cursor;
    }
    state.scroll = state.scroll.min(total.saturating_sub(state.viewport_rows));

    let first_row = state.scroll;
    let mut covered = stack_composited(pane, frame).then(ApcScene::new);
    let scene = covered.as_mut().unwrap_or(scene);
    paint_stack_rows(
        state, editors, buffers, is_focused, text_style, undercurls, frame, buf, scene, first_row,
        inner,
    );
}

/// Whether the terminal composites the resident pool of `pane`'s stack over
/// the rect the stack paints into.
///
/// The rect is the pane's content area less its minimap strip, the rect that
/// [`diff_changes_pool_panes`](crate::apc_emit::diff_changes_pool_panes)
/// declares. A pool that the terminal does not hold yet, or holds at another
/// rect, covers no part of the paint.
pub(crate) fn stack_composited(pane: &Pane, frame: FrameCtx<'_>) -> bool {
    let (inner, _) = split_pane_status(pane.area);
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled);
    frame.composited_stacks.contains(&(pane.index, inner))
}

/// Paint stack rows `first_row..first_row + area.height` of the stack `state`
/// holds into `area` of `buf`.
///
/// Each excerpt the rows reach takes a header row in the commit header style,
/// with its file name centered, and then its rows. The rows paint from the
/// excerpt's own scroll row, so an excerpt cut at the top of `area` shows its
/// lower rows. A painted excerpt's editor records where this paint laid it
/// out: its scroll row, its viewport, its text rect, and the cell of its
/// cursor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_stack_rows(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    is_focused: bool,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    first_row: usize,
    area: Rect,
) {
    let header_style = frame.theme.get(scope::VCS_COMMIT_SHA);
    let (scroll, rows_on_screen) = (first_row, usize::from(area.height));
    for (index, excerpt) in state.excerpts.iter().enumerate() {
        let rows = diff_changes::excerpt_rows(excerpt, editors);
        let top = excerpt.top;
        if top + 1 + rows <= scroll || top >= scroll + rows_on_screen {
            continue;
        }
        if top >= scroll {
            let y = area.y + (top - scroll) as u16;
            let end_x = area.x + area.width;
            let width = text_width(&excerpt.rel_path).min(usize::from(area.width)) as u16;
            let x = area.x + (area.width - width) / 2;
            write_str_clipped(buf, x, y, &excerpt.rel_path, header_style, end_x);
        }

        // The screen row of the excerpt's first row, past its header.
        let first = (top + 1).saturating_sub(scroll);
        let Some(editor) = editors.get_mut(excerpt.editor_id) else {
            continue;
        };
        if rows == 0 || first >= rows_on_screen {
            continue;
        }
        editor.scroll_row = scroll.saturating_sub(top + 1) as u32;
        let height = (rows - editor.scroll_row as usize).min(rows_on_screen - first);
        let excerpt_area = Rect {
            x: area.x,
            y: area.y + first as u16,
            width: area.width,
            height: height as u16,
        };
        let focused = is_focused && index == state.focused;
        let diagnostic_info = buffers
            .path_for(editor.buffer_id)
            .map(|path| (path, frame.diagnostics));
        render_editor_with_overlay(
            editor,
            excerpt_area,
            text_style,
            frame.theme,
            frame.chrome,
            buf,
            focused,
            false,
            frame.line_numbers,
            frame.mode == "insert",
            if focused { frame.hover_cell } else { None },
            if focused {
                frame.goto_word_labels
            } else {
                None
            },
            frame.search_query,
            frame.search_smart_case,
            diagnostic_info,
            match scene.live() {
                true => Some(&mut *scene),
                false => None,
            },
            frame.terminal_cursor_editor == Some(excerpt.editor_id),
            Some(&mut *undercurls),
            0.0,
            frame.diff_dials,
            frame.wrap_mode,
            frame.wrap_column,
        );
        editor.viewport_rows = Some(rows as u32);
    }
}

/// Paint the stack rows from `first_row` over the whole of `page`, as a page of
/// the changes view's pool holds them.
///
/// The rows paint as [`paint_stack_rows`] paints them, with selections, search
/// matches, and diagnostics. Each excerpt's editor keeps the layout the live
/// paint recorded, so a page painted after the live frame leaves the terminal
/// cursor and pointer hits on the live rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_stack_page(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    is_focused: bool,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    page: &mut Buffer,
    scene: &mut ApcScene,
    first_row: usize,
) {
    let live: Vec<(EditorId, PaintLayout)> = state
        .excerpts
        .iter()
        .filter_map(|excerpt| {
            let editor = editors.get(excerpt.editor_id)?;
            Some((excerpt.editor_id, PaintLayout::of(editor)))
        })
        .collect();
    let area = page.area;
    paint_stack_rows(
        state, editors, buffers, is_focused, text_style, undercurls, frame, page, scene, first_row,
        area,
    );
    for (editor_id, layout) in live {
        if let Some(editor) = editors.get_mut(editor_id) {
            layout.restore(editor);
        }
    }
}

/// Record the stack's cursor on a frame that replays the pane's last paint, as
/// [`render_diff_changes`] records it when it paints.
///
/// The paint records which cursor the stack followed. It also lays the focused
/// excerpt out at the stack's scroll and records the cell where the terminal
/// draws its cursor. A replay puts back the cells of an earlier paint at the
/// same scroll. A paint under no key between the two leaves the excerpt laid
/// out at the scroll that paint used, which differs when it scrolled. So the
/// replay lays the excerpt out again from the stack's scroll and the excerpt's
/// place in it, and records the cell there.
///
/// When the focused excerpt is outside the window, or the terminal does not
/// draw its cursor, a replay records no cell. The paint does the same.
pub(crate) fn replay_cursor(
    pane: &Pane,
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    frame: FrameCtx<'_>,
    scene_live: bool,
) {
    if diff_changes::cursor_stack_row(state, editors).is_some() {
        state.followed_cursor = diff_changes::focused_cursor(state, editors);
    }
    let Some(excerpt) = state.focused_excerpt() else {
        return;
    };
    if let Some(editor) = editors.get_mut(excerpt.editor_id) {
        editor.cursor_screen_cell = None;
    }
    if !scene_live || frame.terminal_cursor_editor != Some(excerpt.editor_id) {
        return;
    }

    let (top, scroll, viewport) = (excerpt.top, state.scroll, state.viewport_rows);
    let rows = diff_changes::excerpt_rows(excerpt, editors);
    let first = (top + 1).saturating_sub(scroll);
    if rows == 0 || top + 1 + rows <= scroll || top >= scroll + viewport || first >= viewport {
        return;
    }
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return;
    };
    let Some(text) = editor.text_rect else {
        return;
    };
    let (inner, _) = split_pane_status(pane.area);
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled);
    let scroll_row = scroll.saturating_sub(top + 1);
    let text = Rect {
        y: inner.y + first as u16,
        height: (rows - scroll_row).min(viewport - first) as u16,
        ..text
    };
    editor.scroll_row = scroll_row as u32;
    editor.text_rect = Some(text);

    let point = diff_changes::cursor_point(editor);
    let snapshot = editor.display_map.snapshot();
    let end_row = (editor.scroll_row + u32::from(text.height)).min(snapshot.line_count());
    editor.cursor_screen_cell = screen_cell(
        snapshot.buffer_to_display(point),
        editor.scroll_row,
        end_row,
        text,
    );
}

#[cfg(test)]
mod tests {
    use super::{paint_stack_page, paint_stack_rows};
    use crate::{
        action_handlers, apc_emit,
        app::Stoat,
        buffer_registry::BufferRegistry,
        diff_changes::{self, DiffChangesState},
        editor_state::{EditorId, EditorState},
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::{
            commits::PreviewDials,
            layout::split_pane_status,
            paint,
            review::{self, DiffColumns, DiffDials, DiffLayout},
            themed_blank,
            undercurl::UndercurlBatch,
            FrameCtx, SINGLE_MINIMAP_STRIP_ID,
        },
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::scope,
        workspace::Workspace,
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use slotmap::SlotMap;
    use std::{
        collections::HashMap,
        mem,
        path::{Path, PathBuf},
        sync::LazyLock,
    };
    use stoat_config::{LineNumbers, MinimapMode, WrapMode};
    use stoat_text::Point;
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::Command;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// The strip id, content id, and palette of a minimap strip that a frame
    /// declares.
    type Declared = (u32, u32, Vec<[u8; 3]>);

    /// The first row the changes view paints, as text, from one render that
    /// runs no pool job.
    fn first_row(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The focused changes view's state, with the editors its excerpts are.
    fn stack(h: &mut TestHarness) -> (&mut DiffChangesState, &mut SlotMap<EditorId, EditorState>) {
        let ws = h.stoat.active_workspace_mut();
        let id = ws
            .focused_diff_changes_id()
            .expect("the changes view is open");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        (&mut diff_changes_views[id], editors)
    }

    /// The text in columns `x` to `x + width` of row `y`, trimmed.
    fn cells(buf: &Buffer, x: u16, width: u16, y: u16) -> String {
        (x..x + width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// Two changed files under a focused changes view with no minimap, at a
    /// width that pairs each modified line with its base line, settled.
    fn two_files(height: u16) -> TestHarness {
        changes_view(
            120,
            height,
            &[("a.txt", "a\nx\n", "A\nx\n"), ("b.txt", "b\n", "B\n")],
        )
    }

    /// `files` changed under a focused changes view with no minimap, opened
    /// over the first file, settled.
    fn changes_view(width: u16, height: u16, files: &[(&str, &str, &str)]) -> TestHarness {
        let mut h = TestHarness::with_size(width, height);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stage_review_scenario("/repo", files);
        h.open_file(&Path::new("/repo").join(files[0].0));
        h.cover_with_diff_changes(false);
        h.settle();
        h
    }

    /// Each excerpt paints under a header row that names its file in the middle
    /// of the row, with its rows numbered in the per-file diff's gutters, and
    /// only the focused excerpt paints a cursor.
    #[test]
    fn a_changes_view_paints_its_excerpts_under_their_file_headers() {
        let mut h = two_files(16);
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        let cols = DiffColumns::compute(inner, DiffLayout::DIFF_VIEW);

        // a.txt's three rows sit under its header on row 0, and b.txt's first
        // row under its header on row 4. Each header pairs its name with the
        // column the name starts on.
        let headers = [inner.y, inner.y + 4].map(|y| {
            let start = (inner.x..inner.x + inner.width).find(|&x| buf[(x, y)].symbol() != " ");
            (cells(&buf, inner.x, inner.width, y), start)
        });
        let numbers: Vec<(String, String)> = [1, 2, 3, 5]
            .into_iter()
            .map(|row| {
                (
                    cells(&buf, cols.left_num_x, 4, inner.y + row),
                    cells(&buf, cols.right_num_x, 4, inner.y + row),
                )
            })
            .collect();
        let (state, editors) = stack(&mut h);
        let cursors: Vec<Option<(u16, u16)>> = state
            .excerpts
            .iter()
            .map(|excerpt| editors[excerpt.editor_id].cursor_screen_cell)
            .collect();
        let pair = |n: &str| (n.to_string(), n.to_string());
        let centered = Some(inner.x + (inner.width - 5) / 2);
        assert_eq!(
            (headers, numbers, cursors),
            (
                [
                    ("a.txt".to_string(), centered),
                    ("b.txt".to_string(), centered)
                ],
                vec![pair("1"), pair("2"), pair("3"), pair("1")],
                vec![Some((cols.right_text_x, inner.y + 1)), None],
            ),
        );
    }

    /// A file name wider than the pane starts at the pane's left column and
    /// clips at its right edge.
    #[test]
    fn a_header_wider_than_the_pane_clips_from_its_left_column() {
        let name = "a-file-name-wider-than-the-pane.txt";
        let mut h = changes_view(30, 16, &[(name, "a\n", "A\n")]);
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };

        assert_eq!(
            cells(&buf, inner.x, inner.width, inner.y),
            name[..usize::from(inner.width)]
        );
    }

    /// With no excerpt the changes view says that its list builds, or that the
    /// list holds no change.
    #[test]
    fn a_changes_view_with_no_excerpt_says_why() {
        let mut building = TestHarness::with_size(100, 16);
        building.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        building.cover_with_diff_changes(false);

        let mut clean = TestHarness::with_size(100, 16);
        clean.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        clean.fake_git().add_repo(PathBuf::from("/repo"));
        clean.cover_with_diff_changes(false);
        clean.settle();

        assert_eq!(
            (first_row(&mut building), first_row(&mut clean)),
            ("building changes...".to_string(), "no changes".to_string()),
        );
    }

    /// A cursor on the last excerpt's last row makes the paint scroll the stack
    /// so that row is the bottom of the view.
    #[test]
    fn the_last_excerpts_last_row_scrolls_the_stack_to_its_end() {
        let mut h = two_files(4);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChangesLast);
        h.render_composited();

        let (state, editors) = stack(&mut h);
        let total = diff_changes::layout_stack(state, editors);
        let cursor = diff_changes::cursor_stack_row(state, editors);
        assert_eq!(
            (state.focused, cursor, state.scroll + state.viewport_rows),
            (1, Some(total - 1), total),
        );
    }

    /// An excerpt whose header scrolled above the view paints from its own
    /// scroll row, so the top row shows the excerpt's row under the scroll.
    #[test]
    fn an_excerpt_cut_at_the_top_paints_from_its_scroll_row() {
        let mut h = TestHarness::with_size(120, 6);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        let base: String = (1..=8).map(|n| format!("{n}\n")).collect();
        let text: String = (1..=8).map(|n| format!("{n}!\n")).collect();
        h.stage_review_scenario("/repo", &[("a.txt", &base, &text)]);
        h.cover_with_diff_changes(false);
        h.settle();
        {
            let (state, editors) = stack(&mut h);
            let editor = &mut editors[state.excerpts[0].editor_id];
            diff_changes::place_cursor(editor, Point::new(4, 0));
            state.scroll = 3;
        }

        let buf = h.render_composited();
        let (state, editors) = stack(&mut h);
        let scroll_row = editors[state.excerpts[0].editor_id].scroll_row;
        let inner = Rect::new(0, 0, 120, 5);
        let cols = DiffColumns::compute(inner, DiffLayout::DIFF_VIEW);
        assert_eq!(
            (
                state.scroll,
                scroll_row,
                cells(&buf, cols.right_num_x, 4, inner.y)
            ),
            (3, 2, "3".to_string()),
        );
    }

    /// The pair names of a frame with no pair open.
    static NO_PAIRS: LazyLock<HashMap<PathBuf, (String, String)>> = LazyLock::new(HashMap::new);

    /// A frame context that paints the stack as the live frame of `stoat`
    /// paints it, under no prompt, popup, or search.
    fn live_frame(stoat: &Stoat) -> FrameCtx<'_> {
        FrameCtx {
            workspace_name: "",
            workspace_root: Path::new(""),
            mode: "normal",
            screen: None,
            theme: &stoat.theme,
            commits_split: None,
            preview_dials: PreviewDials::from_stoat(stoat),
            diff_changes_building: false,
            chrome: &stoat
                .chrome
                .as_ref()
                .expect("a render resolved the chrome")
                .1,
            pending_count: None,
            recording_register: None,
            held_buffers: &[],
            lsp_status_open: false,
            lsp_progress_entries: &[],
            spinner_phase: 0,
            lsp_servers: &[],
            diff_warm_busy: false,
            repo_change_counts: None,
            hunk_position: None,
            diff_base: None,
            diff_sides: ("", ""),
            pair_sides: &NO_PAIRS,
            lsp_pending: None,
            lsp_message: None,
            status_message: None,
            goto_word_labels: None,
            mode_label: ("", Color::Reset),
            diagnostics: &stoat.diagnostics,
            search_query: None,
            search_smart_case: true,
            status_prompt: None,
            terminal_cursor_editor: stoat.focused_editor_ids().map(|(id, _)| id),
            composited_stacks: &[],
            line_numbers: LineNumbers::Relative,
            wrap_mode: WrapMode::EditorWidth,
            wrap_column: 80,
            inactive_dim: 0.0,
            diff_dials: DiffDials::from_stoat(stoat),
            images_capable: false,
            minimap_enabled: false,
            minimap_chrome: None,
            minimap_band: None,
            badge_cover: None,
            hover_cell: None,
            home: None,
            #[cfg(feature = "perf")]
            perf: None,
        }
    }

    /// Run `draw` over the focused changes view's stack beside a frame context
    /// that paints as the live frame does, with a live scene.
    ///
    /// The context borrows the whole `Stoat`, so the editors leave the
    /// workspace for the paint and return after it.
    fn paint_directly<R>(
        h: &mut TestHarness,
        draw: impl FnOnce(
            &DiffChangesState,
            &mut SlotMap<EditorId, EditorState>,
            &BufferRegistry,
            FrameCtx<'_>,
            &mut ApcScene,
        ) -> R,
    ) -> R {
        let mut editors = mem::take(&mut h.stoat.active_workspace_mut().editors);
        let mut scene = ApcScene::new();
        scene.set_live(true);
        let out = {
            let ws = h.stoat.active_workspace();
            let id = ws
                .focused_diff_changes_id()
                .expect("the changes view is open");
            draw(
                &ws.diff_changes_views[id],
                &mut editors,
                &ws.buffers,
                live_frame(&h.stoat),
                &mut scene,
            )
        };
        h.stoat.active_workspace_mut().editors = editors;
        out
    }

    /// A run of stack rows painted on its own shows what the live paint shows
    /// on those rows. The run starts inside a.txt, so that excerpt paints from
    /// its second row, and b.txt's header falls inside the run.
    #[test]
    fn a_run_of_stack_rows_paints_what_the_live_paint_shows() {
        let mut h = two_files(16);
        let live = h.stoat.render();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        let run = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut run = Buffer::empty(Rect::new(0, 0, inner.width, 4));
            let area = run.area;
            let text_style = frame.theme.get(scope::UI_TEXT);
            let mut undercurls = UndercurlBatch::default();
            paint_stack_rows(
                state,
                editors,
                buffers,
                true,
                text_style,
                &mut undercurls,
                frame,
                &mut run,
                scene,
                2,
                area,
            );
            run
        });

        let rows = |buf: &Buffer, x: u16, y: u16| -> Vec<String> {
            (y..y + 4).map(|y| cells(buf, x, inner.width, y)).collect()
        };
        let expected = rows(&live, inner.x, inner.y + 2);
        assert_eq!(
            (rows(&run, 0, 0), expected[2].as_str()),
            (expected.clone(), "b.txt"),
        );
    }

    /// A page painted after the live frame leaves each excerpt's editor laid
    /// out as the live paint left it, so the terminal cursor and pointer hits
    /// stay on the live rows. The page starts at a.txt's second row, where the
    /// paint lays a.txt out from another scroll row and cuts its cursor's row.
    #[test]
    fn a_stack_page_leaves_the_live_layout() {
        let mut h = two_files(16);
        h.stoat.render();
        let layouts = |h: &mut TestHarness| {
            let (state, editors) = stack(h);
            state
                .excerpts
                .iter()
                .map(|excerpt| {
                    let editor = &editors[excerpt.editor_id];
                    (
                        editor.scroll_row,
                        editor.viewport_rows,
                        editor.cursor_screen_cell,
                        editor.text_rect,
                        editor.minimap_rect,
                    )
                })
                .collect::<Vec<_>>()
        };
        let live = layouts(&mut h);

        let header = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut page = Buffer::empty(Rect::new(0, 0, 120, 4));
            let text_style = frame.theme.get(scope::UI_TEXT);
            let mut undercurls = UndercurlBatch::default();
            paint_stack_page(
                state,
                editors,
                buffers,
                true,
                text_style,
                &mut undercurls,
                frame,
                &mut page,
                scene,
                2,
            );
            cells(&page, 0, 120, 2)
        });

        assert_eq!(
            (live[0].2.is_some(), header.as_str(), layouts(&mut h)),
            (true, "b.txt", live.clone()),
            "the live cursor cell, the page's header row, and every layout field",
        );
    }

    /// Two staged files under a focused changes view at 120 by 24, with the
    /// minimap in `mode`, on a stoatty of protocol 13.
    fn strip_harness(mode: MinimapMode) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = TestHarness::with_size(120, 24);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        h.stoat.settings.editor_minimap = Some(mode);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle();
        (h, rx)
    }

    /// Paint one frame, and return its cells, the strip rect the focused
    /// changes view reserved, and every minimap strip the frame declared.
    fn paint_strips(
        h: &mut TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
    ) -> (Buffer, Option<Rect>, Vec<Declared>) {
        let buf = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let declared = drain_apc(rx)
            .into_iter()
            .filter_map(|command| match command {
                Command::Minimap(strip) => Some((strip.strip_id, strip.content_id, strip.palette)),
                _ => None,
            })
            .collect();
        let reserved = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.minimap_rect);
        (buf, reserved, declared)
    }

    /// The workspace's changes view strip as `strip_id` declares it, in the
    /// diff palette that the dials derive, blended toward the background by
    /// `dim`.
    fn diff_changes_strip(h: &TestHarness, strip_id: u32, dim: f32) -> Declared {
        let bg = paint::style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg)
            .expect("the test theme has an rgb background");
        let palette = review::diff_minimap_palette(
            &h.stoat.minimap_class_table,
            &h.stoat.theme,
            DiffDials::from_stoat(&h.stoat),
            h.stoat.diff_syntax,
        )
        .into_iter()
        .map(|color| paint::dim_rgb(color, bg, dim))
        .collect();
        let content_id = h.stoat.diff_changes_strips[&h.stoat.active_workspace].content_id();
        (strip_id, content_id, palette)
    }

    fn focused_pane_index(h: &TestHarness) -> u32 {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).index
    }

    /// A per-pane changes view gives its right edge to the minimap strip,
    /// leaves the strip's cells blank, and declares the strip in the diff
    /// palette.
    #[test]
    fn a_per_pane_changes_view_declares_its_strip_in_the_diff_palette() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let (buf, reserved, declared) = paint_strips(&mut h, &mut rx);
        let strip = Rect::new(112, 0, 8, 23);
        let blank = themed_blank(&h.stoat.theme);
        let painted: Vec<_> = strip
            .positions()
            .filter(|&cell| buf[cell] != blank)
            .collect();
        assert_eq!(
            (reserved, painted, declared),
            (
                Some(strip),
                vec![],
                vec![diff_changes_strip(&h, focused_pane_index(&h), 0.0)]
            ),
        );
    }

    /// A per-pane changes view that a dock took focus from declares its strip
    /// in the diff palette at the inactive dim.
    #[test]
    fn an_unfocused_per_pane_changes_view_dims_its_strip() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let index = focused_pane_index(&h);
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);

        let (_, _, declared) = paint_strips(&mut h, &mut rx);
        // 0.25 is the default ui.inactive_dim the test harness resolves.
        assert_eq!(declared, [diff_changes_strip(&h, index, 0.25)]);
    }

    /// The single band shows a focused changes view's strip in the diff
    /// palette, and the pane reserves no strip of its own.
    #[test]
    fn the_single_band_follows_a_focused_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, reserved, declared) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (reserved, declared),
            (
                None,
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)]
            ),
        );
    }
}
