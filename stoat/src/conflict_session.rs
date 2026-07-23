use crate::{
    editor_state::EditorId,
    merge_view::{MergeDoc, RowPick},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use stoat_action::ActionKind;
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
    /// Arms the two-press guard that protects a hand-edited chunk from being
    /// silently overwritten by a whole-side pick.
    ///
    /// A pick over a chunk whose current text matches no pick (a
    /// [`crate::merge_view::ChunkState::Manual`] region) sets this to that
    /// chunk and the pick's action, then warns instead of editing. An
    /// immediate repeat of the identical pick clears it and overwrites. Any
    /// different pick re-arms for the new target.
    pub(crate) pending_clobber: Option<(usize, ActionKind)>,
    /// In-progress resolve state of files visited and stepped away from, keyed
    /// by their index in [`Self::files`].
    ///
    /// Stepping to another file parks the outgoing [`Self::file`] here rather
    /// than rebuilding it, so returning restores its picks and scratch buffer.
    /// Each parked entry keeps its center editor and buffer alive until the view
    /// closes.
    pub(crate) parked: HashMap<usize, FileResolveState>,
    /// Indices in [`Self::files`] whose resolution has been written and marked
    /// resolved in the index.
    ///
    /// Apply records the current file here, then advances to the next index not
    /// in this set, closing the view once every file is applied.
    pub(crate) applied: HashSet<usize>,
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
#[derive(Clone)]
pub(crate) struct ConflictViewState {
    pub(crate) doc: MergeDoc,
    /// Start/end anchors of each chunk's center region, so the renderer can
    /// resolve the current row span of a chunk (which shrinks when a pick
    /// reassembles the region) and align the side columns to it.
    pub(crate) chunk_anchors: Vec<(Anchor, Anchor)>,
    /// Per-chunk row picks mirrored from the session, so the renderer can
    /// derive each chunk's resolution state against its live region text and
    /// paint the matching gutter glyph.
    pub(crate) picks: Vec<Vec<RowPick>>,
    pub(crate) file_index: usize,
    pub(crate) file_count: usize,
    pub(crate) rel_path: String,
}

#[cfg(test)]
mod tests {
    use crate::{app::Stoat, merge_view::MergeDoc, test_harness::TestHarness};
    use stoat_action::{
        Action, CloseConflict, Conflict, ConflictApply, ConflictNextChunk, ConflictNextFile,
        ConflictPickBoth, ConflictPickOurs, ConflictPickOursLine, ConflictPickTheirs,
        ConflictPickTheirsLine, ConflictPrevChunk, ConflictPrevFile, ConflictResetChunk,
        GotoNextChange, GotoPrevChange,
    };

    const MARKER: &str = "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n";

    fn dispatch_conflict(h: &mut TestHarness) {
        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
    }

    /// Open the seeded conflict view with the cursor already landed on the
    /// single chunk.
    fn open(h: &mut TestHarness) {
        seed_conflict(h);
        dispatch_conflict(h);
    }

    fn pick(h: &mut TestHarness, action: &dyn Action) {
        crate::action_handlers::dispatch(&mut h.stoat, action);
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

    /// Seed one file whose single chunk spans two conflict rows, so a line-level
    /// pick decides one row while the other stays in the marker block. Returns
    /// the marker-block center text the opened view shows.
    fn seed_rows(h: &mut TestHarness) -> String {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb1\nb2\nc\n"),
            Some("a\nO1\nO2\nc\n"),
            Some("a\nT1\nT2\nc\n"),
        );
        MergeDoc::build("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n", None)
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

    /// Seed one file carrying two conflict chunks separated by a clean line, so
    /// navigation has more than one target.
    fn seed_two_chunks(h: &mut TestHarness) {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );
    }

    /// Seed a repository with two conflicted files, each a single chunk, so
    /// file navigation has more than one target.
    fn seed_two_files(h: &mut TestHarness) {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git()
            .add_repo(git_root)
            .conflicted_file("a.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"))
            .conflicted_file("b.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"));
    }

    fn current_file(h: &TestHarness) -> usize {
        h.stoat
            .active_workspace()
            .conflict
            .as_ref()
            .expect("session open")
            .current
    }

    fn cursor_row(h: &mut TestHarness) -> u32 {
        let editor_id = h
            .stoat
            .active_workspace()
            .conflict
            .as_ref()
            .expect("session open")
            .file
            .editor_id;
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("center editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let offset = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().start);
        buffer_snapshot.rope().offset_to_point(offset).row
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
    fn pick_ours_replaces_the_chunk_with_the_ours_side() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &ConflictPickOurs);

        assert_eq!(center_text(&h), "ours\n");
    }

    #[test]
    fn re_picking_switches_the_resolved_side() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(center_text(&h), "ours\n");

        pick(&mut h, &ConflictPickTheirs);
        assert_eq!(center_text(&h), "theirs\n");

        pick(&mut h, &ConflictPickBoth);
        assert_eq!(center_text(&h), "ours\ntheirs\n");
    }

    #[test]
    fn reset_restores_the_marker_block() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &ConflictPickOurs);
        pick(&mut h, &ConflictResetChunk);

        assert_eq!(center_text(&h), MARKER);
    }

    #[test]
    fn undo_reverts_a_pick() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(center_text(&h), "ours\n");

        h.type_keys("u");
        assert_eq!(center_text(&h), MARKER);
    }

    #[test]
    fn pick_theirs_line_moves_one_row_into_the_result() {
        let mut h = Stoat::test();
        seed_rows(&mut h);
        dispatch_conflict(&mut h);

        // Land on the theirs marker line for the first conflict row (T1).
        h.type_keys("j j j j");
        pick(&mut h, &ConflictPickTheirsLine);

        assert_eq!(
            center_text(&h),
            "a\nT1\n<<<<<<< ours\nO2\n=======\nT2\n>>>>>>> theirs\nc\n"
        );
    }

    #[test]
    fn toggling_a_line_pick_off_restores_the_marker_block() {
        let mut h = Stoat::test();
        let marker = seed_rows(&mut h);
        dispatch_conflict(&mut h);

        h.type_keys("j j j j");
        pick(&mut h, &ConflictPickTheirsLine);
        // The pick parks the cursor at the region start, now the T1 result line.
        pick(&mut h, &ConflictPickTheirsLine);

        assert_eq!(center_text(&h), marker);
    }

    #[test]
    fn ours_line_on_a_theirs_marker_line_is_a_hinted_no_op() {
        let mut h = Stoat::test();
        let marker = seed_rows(&mut h);
        dispatch_conflict(&mut h);

        h.type_keys("j j j j");
        pick(&mut h, &ConflictPickOursLine);

        assert_eq!(center_text(&h), marker, "O does not pick a theirs line");
    }

    #[test]
    fn ours_line_in_a_hand_edited_region_inserts_an_aligned_line() {
        let mut h = Stoat::test();
        seed_rows(&mut h);
        dispatch_conflict(&mut h);

        // Land on O1 and hand-edit it so the region classifies as Manual.
        h.type_keys("j");
        h.type_keys("i x escape");
        let before = center_text(&h);

        pick(&mut h, &ConflictPickOursLine);
        let after = center_text(&h);

        assert_eq!(
            after.lines().count(),
            before.lines().count() + 1,
            "one line inserted"
        );
        assert!(
            after.contains("<<<<<<< ours") && after.contains("xO1"),
            "the markers and the hand edit survive, so the region stays manual"
        );
        let ours = |s: &str| s.matches("O1").count() + s.matches("O2").count();
        assert_eq!(
            ours(&after),
            ours(&before) + 1,
            "an aligned ours line was inserted"
        );
    }

    #[test]
    fn pick_over_a_hand_edit_warns_then_overwrites_on_repeat() {
        let mut h = Stoat::test();
        open(&mut h);

        h.type_keys("i z escape");
        let edited = center_text(&h);
        assert_ne!(edited, MARKER, "hand edit changed the chunk");

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(
            center_text(&h),
            edited,
            "first pick over a hand edit warns without overwriting"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("chunk was hand-edited, repeat the pick to overwrite")
        );

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(
            center_text(&h),
            "ours\n",
            "repeat of the identical pick overwrites the hand edit"
        );
    }

    #[test]
    fn an_intervening_action_disarms_the_clobber_guard() {
        let mut h = Stoat::test();
        open(&mut h);

        h.type_keys("i z escape");
        let edited = center_text(&h);

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(
            center_text(&h),
            edited,
            "first press over a hand edit warns"
        );

        pick(&mut h, &ConflictNextChunk);
        pick(&mut h, &ConflictPickOurs);
        assert_eq!(
            center_text(&h),
            edited,
            "an intervening action disarms the guard, so the repeat only warns"
        );
    }

    #[test]
    fn n_and_p_step_between_chunks_without_wrapping() {
        let mut h = Stoat::test();
        seed_two_chunks(&mut h);
        dispatch_conflict(&mut h);
        assert_eq!(cursor_row(&mut h), 1, "opens on the first chunk");

        pick(&mut h, &ConflictNextChunk);
        assert_eq!(cursor_row(&mut h), 7, "n steps to the second chunk");

        pick(&mut h, &ConflictNextChunk);
        assert_eq!(cursor_row(&mut h), 7, "n at the last chunk does not wrap");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no more conflicts"),
            "hitting the end reports it"
        );

        pick(&mut h, &ConflictPrevChunk);
        assert_eq!(cursor_row(&mut h), 1, "p steps back to the first chunk");

        pick(&mut h, &ConflictPrevChunk);
        assert_eq!(cursor_row(&mut h), 1, "p at the first chunk does not wrap");
    }

    #[test]
    fn git_change_navigation_steps_conflict_chunks() {
        let mut h = Stoat::test();
        seed_two_chunks(&mut h);
        dispatch_conflict(&mut h);
        assert_eq!(cursor_row(&mut h), 1, "opens on the first chunk");

        // The git hunk-jump chords (space g n, git_pin n, ]c) all dispatch
        // GotoNextChange, which walks conflict chunks in the conflict view
        // rather than no-opping against the scratch center's absent diff map.
        pick(&mut h, &GotoNextChange);
        assert_eq!(
            cursor_row(&mut h),
            7,
            "GotoNextChange steps to the next chunk"
        );

        pick(&mut h, &GotoPrevChange);
        assert_eq!(cursor_row(&mut h), 1, "GotoPrevChange steps back");
    }

    #[test]
    fn stepping_between_files_parks_and_restores_picks() {
        let mut h = Stoat::test();
        seed_two_files(&mut h);
        dispatch_conflict(&mut h);
        assert_eq!(current_file(&h), 0, "opens on the first file");

        pick(&mut h, &ConflictPickOurs);
        assert_eq!(center_text(&h), "ours\n");

        pick(&mut h, &ConflictNextFile);
        assert_eq!(current_file(&h), 1, "N advances to the second file");
        assert_eq!(center_text(&h), MARKER, "the second file opens unresolved");

        pick(&mut h, &ConflictPickTheirs);
        assert_eq!(center_text(&h), "theirs\n");

        pick(&mut h, &ConflictNextFile);
        assert_eq!(current_file(&h), 1, "N at the last file does not wrap");

        pick(&mut h, &ConflictPrevFile);
        assert_eq!(current_file(&h), 0, "P returns to the first file");
        assert_eq!(
            center_text(&h),
            "ours\n",
            "the first file's pick was parked"
        );

        pick(&mut h, &ConflictPrevFile);
        assert_eq!(current_file(&h), 0, "P at the first file does not wrap");
    }

    #[test]
    fn apply_writes_the_resolved_file_and_marks_it_resolved() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        seed_conflict(&mut h);
        dispatch_conflict(&mut h);
        let path = git_root.join("f.txt");

        pick(&mut h, &ConflictPickOurs);
        pick(&mut h, &ConflictApply);

        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "view closes once the only file is applied"
        );
        let mut written = Vec::new();
        h.stoat
            .fs_host
            .read(&path, &mut written)
            .expect("resolved file written");
        assert_eq!(String::from_utf8(written).unwrap(), "ours\n");
        assert_eq!(h.fake_git().resolved_paths(&git_root), vec![path]);
    }

    #[test]
    fn apply_writes_honest_markers_when_a_chunk_is_unresolved() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        seed_conflict(&mut h);
        dispatch_conflict(&mut h);
        let path = git_root.join("f.txt");

        pick(&mut h, &ConflictApply);

        assert_eq!(
            h.stoat.current_view(),
            Some("conflict"),
            "view stays open on an unresolved file"
        );
        let mut written = Vec::new();
        h.stoat
            .fs_host
            .read(&path, &mut written)
            .expect("marker file written");
        assert_eq!(
            String::from_utf8(written).unwrap(),
            MARKER,
            "the honest marker block is written"
        );
        assert!(
            h.fake_git().resolved_paths(&git_root).is_empty(),
            "an unresolved file is not marked resolved"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("written with 1 unresolved chunk(s)")
        );
    }

    #[test]
    fn apply_advances_to_the_next_unresolved_file() {
        let mut h = Stoat::test();
        seed_two_files(&mut h);
        dispatch_conflict(&mut h);

        pick(&mut h, &ConflictPickOurs);
        pick(&mut h, &ConflictApply);
        assert_eq!(
            h.stoat.current_view(),
            Some("conflict"),
            "view stays open with a file left"
        );
        assert_eq!(current_file(&h), 1, "apply advances to the next file");

        pick(&mut h, &ConflictPickTheirs);
        pick(&mut h, &ConflictApply);
        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "view closes when every file is applied"
        );
    }

    #[test]
    fn snapshot_picked_chunk_pads_the_taller_side() {
        let mut h = TestHarness::with_size(150, 20);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("start\nb\nc\nend\n"),
            Some("start\nO\nend\n"),
            Some("start\nT1\nT2\nend\n"),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        crate::action_handlers::dispatch(&mut h.stoat, &ConflictPickOurs);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        assert_eq!(
            center_text(&h),
            "start\nO\nend\n",
            "picking ours shrinks the center"
        );
        h.assert_snapshot("conflict_view_padded_pick");
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
