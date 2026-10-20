//! The paint of a [`View::Overview`](crate::pane::View::Overview) pane.
//!
//! The overview shows its document through the commit preview's painter, so a
//! hunk reads the same here as in a commit's preview. A cursor row marks where
//! the overview's keys act.

use crate::{
    overview::OverviewState,
    pane::Pane,
    render::{
        commits::{preview_row_count, render_commit_preview},
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

/// Paint the overview `state` shows into `pane`, with its own status row.
///
/// With `doc` the hunks paint from the scroll of `state`, and the cursor row
/// takes the selection background under the diff colors. The cursor clamps to
/// the document's last row, so a smaller rebuild never leaves it past the end.
///
/// With no document the pane says "building overview..." while a build is on
/// the pool, and "no changes" when the last build found none.
pub(crate) fn render_overview(
    pane: &Pane,
    is_focused: bool,
    state: &mut OverviewState,
    doc: Option<&Arc<DiffDocument>>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(
        status_area,
        is_focused,
        Some("overview"),
        frame,
        buf,
        &mut *scene,
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(doc) = doc else {
        let text = match frame.overview_building {
            true => "building overview...",
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
        action_handlers::dispatch, render::commits::preview_row_count, test_harness::TestHarness,
    };
    use std::path::{Path, PathBuf};

    /// The first row the overview paints, as text, from one render that runs
    /// no pool job.
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
    fn an_overview_paints_its_document_under_a_cursor_row() {
        let mut h = TestHarness::with_size(100, 16);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_overview(false);
        h.settle();
        h.assert_snapshot("overview_two_files");

        dispatch(&mut h.stoat, &stoat_action::OverviewNextChunk);
        h.assert_snapshot("overview_cursor_on_second_chunk");
    }

    /// With no document the overview says that a build runs, or that the last
    /// build found no change.
    #[test]
    fn an_overview_with_no_document_says_why() {
        let mut building = TestHarness::with_size(100, 16);
        building.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        building.cover_with_overview(false);

        let mut clean = TestHarness::with_size(100, 16);
        clean.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        clean.fake_git().add_repo(PathBuf::from("/repo"));
        clean.cover_with_overview(false);
        clean.settle();

        assert_eq!(
            (first_row(&mut building), first_row(&mut clean)),
            ("building overview...".to_string(), "no changes".to_string()),
        );
    }

    /// A cursor past the document's end, as a smaller rebuild leaves it, lands
    /// on the last row, and the scroll puts that row at the bottom of the view.
    #[test]
    fn a_cursor_past_the_end_lands_on_the_last_row_in_view() {
        let mut h = TestHarness::with_size(100, 4);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.cover_with_overview(false);
        h.settle();
        let rows = h
            .stoat
            .active_workspace()
            .overview_doc
            .as_ref()
            .and_then(|built| built.doc.as_deref())
            .map_or(0, preview_row_count);
        h.stoat
            .active_workspace_mut()
            .focused_overview_mut()
            .expect("the overview is open")
            .cursor = 99;

        h.render_composited();
        let placed = h
            .stoat
            .active_workspace()
            .focused_overview()
            .map(|state| (state.cursor, state.scroll_top + state.viewport_rows));
        assert_eq!(placed, Some((rows - 1, rows)));
    }
}
