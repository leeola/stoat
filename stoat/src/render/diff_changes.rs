//! The paint of a [`View::DiffChanges`](crate::pane::View::DiffChanges) pane.
//!
//! The changes view shows its document through the commit preview's painter, so
//! a hunk reads the same here as in a commit's preview. A cursor row marks
//! where the changes view's keys act.

use crate::{
    diff_changes::DiffChangesState,
    pane::Pane,
    render::{
        commits::{preview_row_count, render_commit_preview},
        editor::split_minimap_strip,
        layout::split_pane_status,
        pane::render_overlay_status,
        text::write_str,
        FrameCtx,
    },
    review_session::DiffDocument,
    theme::scope,
};
use ratatui::buffer::Buffer;
use std::sync::Arc;
use stoat_widgets::ApcScene;

/// Paint the changes view `state` shows into `pane`, with its own status row.
///
/// With `doc` the hunks paint from the scroll of `state`, and the cursor row
/// takes the selection background under the diff colors. The cursor clamps to
/// the document's last row, so a smaller rebuild never leaves it past the end.
///
/// With no document the pane says "building changes..." while a build is on
/// the pool, and "no changes" when the last build found none.
///
/// With per-pane minimaps on, a pane wide enough for the minimap strip gives
/// its right edge to the strip, with or without a document. The strip's rect
/// lands on [`DiffChangesState::minimap_rect`].
pub(crate) fn render_diff_changes(
    pane: &Pane,
    is_focused: bool,
    state: &mut DiffChangesState,
    doc: Option<&Arc<DiffDocument>>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        Some("diff_changes"),
        frame,
        buf,
        &mut *scene,
    );
    let (inner, strip) = split_minimap_strip(inner, frame.minimap_enabled);
    state.minimap_rect = strip;
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(doc) = doc else {
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
    };

    state.viewport_rows = inner.height as usize;
    state.cursor = state.cursor.min(preview_row_count(doc).saturating_sub(1));
    state.ensure_cursor_visible(state.viewport_rows);
    render_commit_preview(
        doc,
        frame.theme,
        inner,
        state.scroll_top,
        frame.preview_dials,
        buf,
        scene,
    );

    // The background alone changes, so the row keeps its diff colors.
    if let Some(bg) = frame.theme.get(scope::UI_SELECTION).bg {
        let y = inner.y + (state.cursor - state.scroll_top) as u16;
        for x in inner.x..inner.x + inner.width {
            buf[(x, y)].set_bg(bg);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        apc_emit,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::{
            commits::preview_row_count,
            paint,
            review::{self, DiffDials},
            themed_blank, SINGLE_MINIMAP_STRIP_ID,
        },
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::scope,
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::path::{Path, PathBuf};
    use stoat_config::MinimapMode;
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

    /// The cursor row moves with a step to the second chunk.
    #[test]
    fn a_changes_view_paints_its_document_under_a_cursor_row() {
        let mut h = TestHarness::with_size(100, 16);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle();
        h.assert_snapshot("diff_changes_two_files");

        dispatch(&mut h.stoat, &stoat_action::DiffChangesNextChunk);
        h.assert_snapshot("diff_changes_cursor_on_second_chunk");
    }

    /// With no document the changes view says that a build runs, or that the
    /// last build found no change.
    #[test]
    fn a_changes_view_with_no_document_says_why() {
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

    /// A cursor past the document's end, as a smaller rebuild leaves it, lands
    /// on the last row, and the scroll puts that row at the bottom of the view.
    #[test]
    fn a_cursor_past_the_end_lands_on_the_last_row_in_view() {
        let mut h = TestHarness::with_size(100, 4);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.cover_with_diff_changes(false);
        h.settle();
        let rows = h
            .stoat
            .active_workspace()
            .diff_changes_doc
            .as_ref()
            .and_then(|built| built.doc.as_deref())
            .map_or(0, preview_row_count);
        h.stoat
            .active_workspace_mut()
            .focused_diff_changes_mut()
            .expect("the changes view is open")
            .cursor = 99;

        h.render_composited();
        let placed = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .map(|state| (state.cursor, state.scroll_top + state.viewport_rows));
        assert_eq!(placed, Some((rows - 1, rows)));
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
