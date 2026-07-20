use crate::{
    editor_state::EditorId,
    merge_view::{MergeDoc, RowPick},
};
use std::path::PathBuf;
use stoat_text::{Anchor, BufferId};

/// A live three-way conflict resolution across a repository's conflicted files.
///
/// Owns the swapped-in center editor and the original editor displaced from the
/// pane, so closing the view restores the plain file. Held as
/// [`crate::workspace::Workspace::conflict`] while the `:conflict` view is open.
#[allow(dead_code)]
pub(crate) struct ConflictSession {
    /// Repository work directory the conflicted paths sit under.
    pub(crate) workdir: PathBuf,
    /// Every conflicted path in the repository, in discovery order.
    pub(crate) files: Vec<PathBuf>,
    /// Index into [`Self::files`] of the file currently shown.
    pub(crate) current: usize,
    /// Resolve state of the file currently shown.
    pub(crate) file: FileResolveState,
    /// The editor displaced from the focused pane when the view opened,
    /// restored there on close.
    pub(crate) saved_editor: EditorId,
}

/// The in-progress resolution of one conflicted file.
#[allow(dead_code)]
pub(crate) struct FileResolveState {
    /// Absolute path of the conflicted file.
    pub(crate) path: PathBuf,
    /// The three-way merge of the file's index stages.
    pub(crate) doc: MergeDoc,
    /// Per-chunk row picks, one entry per chunk sized to its row count.
    pub(crate) picks: Vec<Vec<RowPick>>,
    /// Start and end anchors of each chunk's region in the center buffer.
    pub(crate) chunk_anchors: Vec<(Anchor, Anchor)>,
    /// The center (result) buffer holding the editable merged text.
    pub(crate) buffer_id: BufferId,
    /// The center editor swapped into the pane.
    pub(crate) editor_id: EditorId,
}

/// Per-editor render cache for the three-column conflict view.
///
/// Cloned from the session onto the swapped-in editor at open, so the renderer
/// (which only receives the focused editor) can build its column display list
/// from `doc` each frame. `file_index`/`file_count`/`rel_path` feed the hints
/// footer. Refreshed alongside the session when a pick reassembles a region.
#[allow(dead_code)]
pub(crate) struct ConflictViewState {
    pub(crate) doc: MergeDoc,
    pub(crate) file_index: usize,
    pub(crate) file_count: usize,
    pub(crate) rel_path: String,
}

#[cfg(test)]
mod tests {
    use crate::{app::Stoat, merge_view::MergeDoc, test_harness::TestHarness};
    use stoat_action::{CloseConflict, Conflict};

    fn dispatch_conflict(h: &mut TestHarness) {
        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
    }

    /// Seed one conflicted file at the workspace's git root and return the
    /// center text the opened view should show.
    fn seed_conflict(h: &mut TestHarness) -> String {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("base\n"),
            Some("ours\n"),
            Some("theirs\n"),
        );
        MergeDoc::build("base\n", "ours\n", "theirs\n", None)
            .initial_center_text()
            .0
    }

    fn center_text(h: &TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        let buffer_id = ws.conflict.as_ref().expect("session open").file.buffer_id;
        ws.buffers
            .get(buffer_id)
            .expect("center buffer")
            .read()
            .expect("buffer poisoned")
            .rope()
            .to_string()
    }

    #[test]
    fn conflict_opens_the_resolve_view_on_the_merged_center() {
        let mut h = Stoat::test();
        let expected = seed_conflict(&mut h);

        dispatch_conflict(&mut h);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        assert_eq!(center_text(&h), expected);
    }

    #[test]
    fn conflict_widens_and_toggling_restores_the_file_view() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);
        h.type_keys("space a s");

        let (focused, focused_area, buffers_before) = {
            let panes = &h.stoat.active_workspace().panes;
            let focused = panes.focus();
            (
                focused,
                panes.pane(focused).area,
                h.stoat.active_workspace().buffers.len(),
            )
        };

        dispatch_conflict(&mut h);
        assert_eq!(h.stoat.current_view(), Some("conflict"));
        assert_eq!(
            h.stoat.active_workspace().panes.widened(),
            Some(focused),
            "opening the conflict view widens the focused pane"
        );

        dispatch_conflict(&mut h);
        assert_ne!(
            h.stoat.current_view(),
            Some("conflict"),
            "re-dispatch closes"
        );
        let ws = h.stoat.active_workspace();
        assert_eq!(ws.panes.widened(), None, "closing unwidens");
        assert_eq!(ws.panes.pane(focused).area, focused_area, "pane restored");
        assert_eq!(ws.buffers.len(), buffers_before, "scratch buffer disposed");
    }

    #[test]
    fn close_conflict_restores_the_file_view() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);
        let buffers_before = h.stoat.active_workspace().buffers.len();

        dispatch_conflict(&mut h);
        assert_eq!(h.stoat.current_view(), Some("conflict"));

        crate::action_handlers::dispatch(&mut h.stoat, &CloseConflict);
        assert_ne!(h.stoat.current_view(), Some("conflict"));
        assert_eq!(
            h.stoat.active_workspace().buffers.len(),
            buffers_before,
            "scratch buffer disposed on close"
        );
    }

    #[test]
    fn conflict_without_index_conflicts_sets_a_status() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root);

        dispatch_conflict(&mut h);

        assert_eq!(h.stoat.current_view(), Some("file"));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no merge conflicts")
        );
    }

    #[test]
    fn snapshot_conflict_view_three_columns() {
        let mut h = TestHarness::with_size(150, 20);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "src/f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        h.assert_snapshot("conflict_view_three_columns");
    }

    #[test]
    fn snapshot_conflict_view_narrow_drops_side_gutters() {
        let mut h = TestHarness::with_size(90, 16);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\n"),
            Some("a\nB\nc\n"),
            Some("a\nX\nc\n"),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        h.assert_snapshot("conflict_view_narrow");
    }
}
