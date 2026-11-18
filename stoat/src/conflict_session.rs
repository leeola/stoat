use crate::{
    buffer_registry::BufferRegistry,
    display_map::DisplaySnapshot,
    editor_state::EditorId,
    host::git::{GitRepo, MergeState},
    merge_view::{AlignRow, ChunkState, MergeDoc, RowPick},
    multi_buffer::{MultiBuffer, MultiBufferSnapshot},
    workspace::{diff, Workspace},
};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};
use stoat_action::ActionKind;
use stoat_text::{Anchor, BufferId};

/// A live three-way conflict resolution across a repository's conflicted files.
///
/// Owns the swapped-in center editor and the original editor displaced from the
/// pane, so closing the view restores the plain file. Held as
/// [`crate::workspace::Workspace::conflict`] while the `:conflict` view is open.
pub(crate) struct ConflictSession {
    /// The repository the open discovered, held so an apply marks a file
    /// resolved without rediscovering it per press.
    pub(crate) repo: Arc<dyn GitRepo>,
    /// Every conflicted path in the repository whose stages the open read, in
    /// discovery order.
    pub(crate) files: Vec<PathBuf>,
    /// Whether the conflicts come from a merge in progress, as the open read
    /// the repository.
    ///
    /// Read once, since a merge ends through its abort or its commit, and both
    /// close the view.
    pub(crate) merging: bool,
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
    /// The chunks still on their markers, with the buffer version they were
    /// counted at, so a frame counts a file again only after its text moved.
    pub(crate) open_chunks: Cell<Option<(u64, usize)>>,
}

/// Per-editor render cache for the three-column conflict view.
///
/// Cloned from the session onto the swapped-in editor at open, so the renderer,
/// which receives the focused editor alone, builds its column display list
/// from `doc` each frame. Refreshed alongside the session when a pick
/// reassembles a region.
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
    /// The names the MergeGrid header gives the two sides.
    pub(crate) labels: ConflictLabels,
    /// Layout and chunk classification derived from the center buffer, rebuilt
    /// only when that buffer or [`Self::picks`] changes. See [`Self::derived`].
    derived: Option<ConflictDerived>,
}

/// Everything the three-column painter needs that falls out of the center
/// buffer's current contents rather than being stored.
///
/// Deriving it costs four anchor resolutions and a whole-region `String` per
/// chunk, for every chunk in the file rather than just the visible ones, so it
/// is far too expensive to repeat per frame. `buffer_version` keys the cache to
/// the snapshot it was built from, which keeps the derived-never-stored
/// guarantee intact. A classification can never outlive the text that produced
/// it, so an edit or undo cannot leave a gutter glyph disagreeing with the
/// lines beside it.
#[derive(Clone)]
pub(crate) struct ConflictDerived {
    buffer_version: u64,
    pub(crate) states: Vec<ChunkState>,
    pub(crate) plan: Vec<AlignRow>,
}

impl ConflictViewState {
    pub(crate) fn new(
        doc: MergeDoc,
        chunk_anchors: Vec<(Anchor, Anchor)>,
        picks: Vec<Vec<RowPick>>,
        labels: ConflictLabels,
    ) -> Self {
        Self {
            doc,
            chunk_anchors,
            picks,
            labels,
            derived: None,
        }
    }

    /// The index of the chunk whose center region holds byte `offset` of the
    /// merged buffer `buffer`, or `None` for an offset outside every chunk.
    pub(crate) fn chunk_at(&self, buffer: &MultiBufferSnapshot, offset: usize) -> Option<usize> {
        chunk_holding(&self.chunk_anchors, buffer, offset)
    }

    /// The merge doc paired with its derived layout for `snapshot`, rebuilding
    /// the layout only when the center buffer has changed since the last call.
    ///
    /// The doc rides along because every consumer needs both and the rebuild
    /// takes `&mut self`, so handing them back together is what lets a caller
    /// hold the two at once.
    ///
    /// A pick can rewrite a region to the same text it already held, leaving the
    /// version untouched, so the pick site clears the cache outright rather than
    /// relying on the version to notice.
    pub(crate) fn derived(&mut self, snapshot: &DisplaySnapshot) -> (&MergeDoc, &ConflictDerived) {
        let version = snapshot.buffer_snapshot().version();
        if self
            .derived
            .as_ref()
            .is_none_or(|d| d.buffer_version != version)
        {
            let states = chunk_states(snapshot, &self.doc, &self.chunk_anchors, &self.picks);
            let plan = self
                .doc
                .align(&chunk_center_rows(snapshot, &self.chunk_anchors));
            self.derived = Some(ConflictDerived {
                buffer_version: version,
                states,
                plan,
            });
        }
        (&self.doc, self.derived.as_ref().expect("just populated"))
    }

    /// The document and its layout, when the layout is already current for
    /// `snapshot`.
    ///
    /// For a reader holding the state by shared reference. A pooled page fill
    /// shares it with the editor rather than owning a copy, and the live paint
    /// of the same frame has already built the layout it wants.
    pub(crate) fn derived_current(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> Option<(&MergeDoc, &ConflictDerived)> {
        let version = snapshot.buffer_snapshot().version();
        let derived = self
            .derived
            .as_ref()
            .filter(|d| d.buffer_version == version)?;
        Some((&self.doc, derived))
    }

    /// Drop the derived layout so the next paint rebuilds it.
    pub(crate) fn invalidate_derived(&mut self) {
        self.derived = None;
    }
}

/// The names the conflict view gives the two sides of the merge.
///
/// Read once when the view opens, since a merge keeps both sides fixed until
/// it ends, and every file of the session shares them. Index conflicts with no
/// merge in progress, as a cherry-pick or a stash pop leaves them, have no
/// merge head, so theirs reads `theirs`, the side git's markers name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictLabels {
    /// The branch HEAD is on, `None` for a detached HEAD.
    ours_branch: Option<String>,
    /// The local branch at the merge head, `None` with no merge or no branch
    /// at its head.
    theirs_name: Option<String>,
    /// The merge head's short sha, `None` with no merge.
    theirs_sha: Option<String>,
}

impl ConflictLabels {
    /// The labels for a repository whose HEAD is on `head_branch`, mid-merge of
    /// `merge` when one is in progress.
    pub(crate) fn new(head_branch: Option<String>, merge: Option<&MergeState>) -> Self {
        Self {
            ours_branch: head_branch,
            theirs_name: merge.and_then(|merge| merge.head_name.clone()),
            theirs_sha: merge.map(|merge| diff::short_sha(&merge.head_sha)),
        }
    }

    /// Ours as the MergeGrid's header names it, `HEAD · {branch}`, or `HEAD`
    /// alone on a detached HEAD.
    pub(crate) fn ours_label(&self) -> String {
        match &self.ours_branch {
            Some(branch) => format!("HEAD \u{b7} {branch}"),
            None => self.ours_short().to_string(),
        }
    }

    /// Theirs as the MergeGrid's header names it, `{branch} · {sha}`, or the
    /// short form when no branch is at the merge head or no merge is in
    /// progress.
    pub(crate) fn theirs_label(&self) -> String {
        match (&self.theirs_name, &self.theirs_sha) {
            (Some(name), Some(sha)) => format!("{name} \u{b7} {sha}"),
            _ => self.theirs_short().to_string(),
        }
    }

    /// Ours in one word, HEAD's branch or `HEAD` on a detached HEAD.
    pub(crate) fn ours_short(&self) -> &str {
        self.ours_branch.as_deref().unwrap_or("HEAD")
    }

    /// Theirs in one word, the merge head's branch, else its short sha, else
    /// `theirs` with no merge.
    pub(crate) fn theirs_short(&self) -> &str {
        self.theirs_name
            .as_deref()
            .or(self.theirs_sha.as_deref())
            .unwrap_or("theirs")
    }
}

/// Where the focused conflict view's cursor stands among its file's chunks,
/// where its file stands among the session's files, and how many chunks stay
/// on their markers.
///
/// The status row and the hints footer read it, as they read a diff view's
/// [`HunkPosition`](crate::workspace::diff::HunkPosition).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConflictPosition {
    /// `merge` during a merge, `rebase` during a rebase, and `conflict` for
    /// conflicts with neither behind them.
    pub(crate) operation: &'static str,
    /// The 1-based chunk the cursor stands in, or `None` outside every chunk.
    pub(crate) chunk_at: Option<usize>,
    pub(crate) chunk_total: usize,
    /// The 1-based place of the shown file in the session's file list.
    pub(crate) file_at: usize,
    /// The conflicted files the session lists.
    pub(crate) file_total: usize,
    /// The shown file's chunks still on their markers.
    pub(crate) file_open: usize,
    /// The chunks still on their markers in every file of the session.
    pub(crate) chunks_open: usize,
    /// The files of the session that hold a chunk still on its markers.
    pub(crate) files_open: usize,
}

/// The focused conflict view's [`ConflictPosition`], or `None` when the focused
/// pane shows no conflict view.
///
/// The cursor is the newest selection's start, the place the chunk keys read,
/// so the chunk the position names is the chunk a pick acts on. The open
/// counts read each file's held count, as [`open_chunks`] keeps it.
pub(crate) fn conflict_position(ws: &Workspace) -> Option<ConflictPosition> {
    let session = ws.conflict.as_ref()?;
    let (editor_id, buffer_id) = ws.focused_editor()?;
    let editor = ws.editors.get(editor_id)?;
    let state = editor.conflict_view.as_ref()?;

    let buffer = MultiBuffer::singleton(ws.buffers.get(buffer_id)?).snapshot();
    let cursor = buffer.resolve_anchor(&editor.selections.newest_anchor().start);
    let operation = match (session.merging, ws.rebase_active.is_some()) {
        (true, _) => "merge",
        (false, true) => "rebase",
        (false, false) => "conflict",
    };

    let file_open = open_chunks(&session.file, &ws.buffers);
    let (chunks_open, files_open) = session
        .parked
        .values()
        .map(|file| open_chunks(file, &ws.buffers))
        .chain([file_open])
        .fold((0, 0), |(chunks, files), open| {
            (chunks + open, files + usize::from(open > 0))
        });

    Some(ConflictPosition {
        operation,
        chunk_at: state.chunk_at(&buffer, cursor).map(|index| index + 1),
        chunk_total: state.doc.chunks.len(),
        file_at: session.current + 1,
        file_total: session.files.len(),
        file_open,
        chunks_open,
        files_open,
    })
}

/// The chunks of `file` whose center region still holds its conflict markers.
///
/// Picks and hand edits both take a chunk off the count. The count holds
/// against the center buffer's version, so a later call counts again only
/// after the text moved. A file whose buffer is gone counts none.
pub(crate) fn open_chunks(file: &FileResolveState, buffers: &BufferRegistry) -> usize {
    let Some(buffer) = buffers.get(file.buffer_id) else {
        return 0;
    };
    let buffer = MultiBuffer::singleton(buffer);
    let version = buffer.buffer_version();
    if let Some((held, count)) = file.open_chunks.get()
        && held == version
    {
        return count;
    }

    let snapshot = buffer.snapshot();
    let rope = snapshot.rope();
    let count = file
        .chunk_anchors
        .iter()
        .zip(&file.doc.chunks)
        .zip(&file.picks)
        .filter(|(((start, end), chunk), picks)| {
            let region: String = rope
                .chunks_in_range(snapshot.resolve_anchor(start)..snapshot.resolve_anchor(end))
                .collect();
            chunk.classify(&file.doc.rows, picks, &region) == ChunkState::Unresolved
        })
        .count();
    file.open_chunks.set(Some((version, count)));
    count
}

/// The index of the chunk in `chunk_anchors` whose center region holds byte
/// `offset` of `buffer`, or `None` for an offset outside every chunk.
///
/// A region ends before its end anchor, so the line after a chunk is outside
/// it.
pub(crate) fn chunk_holding(
    chunk_anchors: &[(Anchor, Anchor)],
    buffer: &MultiBufferSnapshot,
    offset: usize,
) -> Option<usize> {
    chunk_anchors.iter().position(|(start, end)| {
        (buffer.resolve_anchor(start)..buffer.resolve_anchor(end)).contains(&offset)
    })
}

/// Resolve each chunk's anchors to the row span it currently occupies in the
/// center buffer.
///
/// A pick reassembles a chunk's marker block into its chosen resolution, which
/// is usually fewer rows, so the span shrinks. Sizing the side band to this live
/// span rather than the original marker height keeps the ours and theirs columns
/// aligned after a pick. Feeds [`MergeDoc::align`].
fn chunk_center_rows(snapshot: &DisplaySnapshot, chunk_anchors: &[(Anchor, Anchor)]) -> Vec<usize> {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    chunk_anchors
        .iter()
        .map(|(start, end)| {
            let start_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(start))
                .row;
            let end_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(end))
                .row;
            end_row.saturating_sub(start_row) as usize
        })
        .collect()
}

/// Classify each chunk against its live center-region text so the gutter glyph
/// and unresolved wash follow picks and hand edits.
fn chunk_states(
    snapshot: &DisplaySnapshot,
    doc: &MergeDoc,
    chunk_anchors: &[(Anchor, Anchor)],
    picks: &[Vec<RowPick>],
) -> Vec<ChunkState> {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    chunk_anchors
        .iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let start = buffer_snapshot.resolve_anchor(start);
            let end = buffer_snapshot.resolve_anchor(end);
            let region_text: String = rope.chunks_in_range(start..end).collect();
            doc.chunks[i].classify(&doc.rows, &picks[i], &region_text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::ConflictLabels;
    use crate::{
        app::Stoat,
        git_status::Note,
        host::git::MergeState,
        merge_view::MergeDoc,
        pane::View,
        rebase::{ActiveRebase, RebaseState},
        test_harness::TestHarness,
    };
    use std::collections::HashMap;
    use stoat_action::{
        Action, CloseConflict, Conflict, ConflictApply, ConflictNextChunk, ConflictNextFile,
        ConflictPickBoth, ConflictPickOurs, ConflictPickOursLine, ConflictPickTheirs,
        ConflictPickTheirsLine, ConflictPrevChunk, ConflictPrevFile, ConflictResetChunk,
        DiffChanges, DiffFiles, FocusLeft, FocusNext, GotoNextChange, GotoPrevChange, NewWorkspace,
        SplitRight,
    };

    const MARKER: &str = "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n";

    /// Open or close the conflict view and settle, since the open leaves the
    /// alignment of the files to a worker and installs the session from the
    /// pump.
    fn dispatch_conflict(h: &mut TestHarness) {
        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        h.settle();
    }

    /// A `width` by `height` harness that paints for a terminal with no scene,
    /// where the conflict view keeps its cell layout.
    fn cell_harness(width: u16, height: u16) -> TestHarness {
        let mut h = TestHarness::with_size(width, height);
        h.stoat.stoatty = false;
        h
    }

    /// Open the seeded conflict view with the cursor already landed on the
    /// single chunk.
    fn open(h: &mut TestHarness) {
        seed_conflict(h);
        dispatch_conflict(h);
    }

    /// Dispatch `action` and settle, since some actions land their work from
    /// a worker or from the git queue, as an apply does.
    fn pick(h: &mut TestHarness, action: &dyn Action) {
        crate::action_handlers::dispatch(&mut h.stoat, action);
        h.settle();
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

    /// Seed one file carrying three conflict chunks, at rows 1, 7, and 13 of the
    /// opened view, so a count of two from the first lands past the second.
    fn seed_three_chunks(h: &mut TestHarness) {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\nd\ne\nf\ng\n"),
            Some("a\nB\nc\nD\ne\nF\ng\n"),
            Some("a\nX\nc\nY\ne\nZ\ng\n"),
        );
        // The harness resolved its root before the repository was seeded.
        h.stoat.watch_active_root();
    }

    /// Seed three conflicted files, each a single chunk, so a count of two from
    /// the first file lands past the second.
    fn seed_three_files(h: &mut TestHarness) {
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git()
            .add_repo(git_root)
            .conflicted_file("a.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"))
            .conflicted_file("b.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"))
            .conflicted_file("c.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"));
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

    /// The press names the pane the view opens in. A focus move before the
    /// alignment lands leaves the view unopened, so the pane the reader moved
    /// to keeps what it shows.
    #[test]
    fn an_open_that_lands_after_the_focus_moved_opens_nothing() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let pressed = h.stoat.active_workspace().panes.focus();
        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        crate::action_handlers::dispatch(&mut h.stoat, &FocusLeft);
        let (moved_to, left_editor) = {
            let panes = &h.stoat.active_workspace().panes;
            let View::Editor(editor) = panes.pane(panes.focus()).view else {
                panic!("the left pane shows an editor");
            };
            (panes.focus(), editor)
        };
        h.settle();

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                moved_to != pressed,
                ws.conflict.is_none(),
                matches!(ws.panes.pane(moved_to).view, View::Editor(editor) if editor == left_editor),
                h.stoat.pending_message.as_deref(),
            ),
            (
                true,
                true,
                true,
                Some("conflict view not opened: focus moved")
            ),
            "the open gives way, and the left pane keeps its editor"
        );
    }

    /// A focus move on the conflict view closes the view and moves nothing.
    #[test]
    fn a_focus_move_on_the_conflict_view_closes_it() {
        let mut h = Stoat::test();
        open(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &FocusNext);

        assert_eq!(
            (
                h.stoat.active_workspace().conflict.is_none(),
                h.stoat.current_view(),
                h.stoat.pending_message.as_deref(),
            ),
            (true, Some("file"), Some("conflict view closed")),
        );
    }

    /// A workspace switch before the alignment lands leaves the view unopened.
    /// Editor ids count within one workspace, so the other workspace's focused
    /// editor carries the same id as the one the press recorded.
    #[test]
    fn an_open_that_lands_in_another_workspace_opens_nothing() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);
        let pressed = h.stoat.active_workspace;
        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        crate::action_handlers::dispatch(&mut h.stoat, &NewWorkspace);
        h.settle();

        assert_eq!(
            (
                h.stoat.active_workspace != pressed,
                h.stoat.workspaces.values().all(|ws| ws.conflict.is_none()),
                h.stoat.pending_message.as_deref(),
            ),
            (true, true, Some("conflict view not opened: focus moved")),
            "the open gives way in both workspaces"
        );
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

    /// `DiffChanges` on the conflict view closes it whole, so the changes view
    /// covers the plain file and its close lands there.
    #[test]
    fn diff_changes_on_the_conflict_view_opens_the_changes_view_over_the_plain_file() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &DiffChanges);
        let opened = (
            h.stoat.active_workspace().conflict.is_some(),
            h.stoat.current_view(),
        );
        pick(&mut h, &DiffChanges);
        assert_eq!(
            (opened, h.stoat.current_view()),
            ((false, Some("diff_changes")), Some("file")),
        );
    }

    #[test]
    fn diff_files_on_the_conflict_view_opens_the_per_file_diff() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(&mut h, &DiffFiles { rev: None });
        assert_eq!(
            (
                h.stoat.active_workspace().conflict.is_some(),
                h.stoat.current_view()
            ),
            (false, Some("diff")),
        );
    }

    /// A revision that the repository does not resolve changes nothing, so the
    /// conflict view and its unwritten resolution stay.
    #[test]
    fn diff_files_with_an_unknown_revision_keeps_the_conflict_view() {
        let mut h = Stoat::test();
        open(&mut h);

        pick(
            &mut h,
            &DiffFiles {
                rev: Some("nope".into()),
            },
        );
        assert_eq!(
            (
                h.stoat.active_workspace().conflict.is_some(),
                h.stoat.current_view()
            ),
            (true, Some("conflict")),
        );
    }

    /// `Conflict` on a changes view closes it whole, so the conflict view opens
    /// over the plain file and its close lands there.
    #[test]
    fn conflict_on_a_changes_view_closes_it_and_opens_the_conflict_view() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);
        h.cover_with_diff_changes(false);

        dispatch_conflict(&mut h);
        let opened = (
            h.stoat.active_workspace().diff_changes_views.is_empty(),
            h.stoat.current_view(),
        );
        dispatch_conflict(&mut h);
        assert_eq!(
            (opened, h.stoat.current_view()),
            ((true, Some("conflict")), Some("file")),
        );
    }

    /// `Conflict` on the per-file diff leaves the diff, so the close lands in
    /// the plain file and not back in the diff.
    #[test]
    fn conflict_on_the_per_file_diff_closes_to_the_plain_file() {
        let mut h = Stoat::test();
        seed_conflict(&mut h);

        pick(&mut h, &DiffFiles { rev: None });
        let diffed = h.stoat.current_view();
        dispatch_conflict(&mut h);
        let opened = h.stoat.current_view();
        dispatch_conflict(&mut h);
        assert_eq!(
            (diffed, opened, h.stoat.current_view()),
            (Some("diff"), Some("conflict"), Some("file")),
        );
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

    /// Discovering the repository and listing its conflicts loads the whole
    /// index, so the press leaves both to the worker. A repository with no
    /// conflicts reports that when the read lands, not at the press.
    #[test]
    fn an_open_reads_the_repository_on_a_worker() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root);

        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                h.stoat.pending_conflict_file.is_some(),
            ),
            (None, true),
            "the press armed a read and reported nothing yet",
        );

        h.settle();
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                h.stoat.pending_conflict_file.is_some(),
            ),
            (Some("no merge conflicts"), false),
            "the landed read reported the empty listing",
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
    fn a_count_steps_that_many_chunks_and_stops_at_the_farthest() {
        let mut h = Stoat::test();
        seed_three_chunks(&mut h);
        dispatch_conflict(&mut h);

        // `] g` is the editor's change walk, which hands its count to the chunk
        // walk inside the conflict view.
        let mut rows = vec![cursor_row(&mut h)];
        for keys in ["2 n", "9 p", "2 ] g"] {
            h.type_keys(keys);
            rows.push(cursor_row(&mut h));
        }

        assert_eq!(
            rows,
            [1, 13, 1, 13],
            "a count steps that many chunks and stops at the farthest one in its direction"
        );
    }

    #[test]
    fn a_count_steps_that_many_files_and_stops_at_either_end() {
        let mut h = Stoat::test();
        seed_three_files(&mut h);
        dispatch_conflict(&mut h);

        let mut files = vec![current_file(&h)];
        for keys in ["2 N", "9 P"] {
            h.type_keys(keys);
            h.settle();
            files.push(current_file(&h));
        }

        assert_eq!(
            files,
            [0, 2, 0],
            "a count steps that many files and stops at either end"
        );
    }

    #[test]
    fn git_change_navigation_steps_conflict_chunks() {
        let mut h = Stoat::test();
        seed_two_chunks(&mut h);
        dispatch_conflict(&mut h);
        assert_eq!(cursor_row(&mut h), 1, "opens on the first chunk");

        // The git hunk-jump chords (space g n, the pinned space_git n, ]c) all dispatch
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

    /// A three-way alignment runs two structural diffs against the ancestor,
    /// which is why the open aligns every file on a worker and leaves the view
    /// as it was until the documents land. A step then shows the next file at
    /// once, since its document landed with the open.
    #[test]
    fn the_open_aligns_every_file_on_a_worker() {
        let mut h = Stoat::test();
        seed_two_files(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &Conflict);
        assert!(
            h.stoat.active_workspace().conflict.is_none(),
            "the open installs no session before the documents land",
        );
        assert!(h.stoat.pending_conflict_file.is_some(), "an open is armed");

        h.settle();
        assert_eq!(current_file(&h), 0, "the settle installs the session");

        crate::action_handlers::dispatch(&mut h.stoat, &ConflictNextFile);
        assert_eq!(
            (current_file(&h), h.stoat.pending_conflict_file.is_none()),
            (1, true),
            "the step shows the second file with nothing armed",
        );
    }

    /// The open lands on the conflicted file the focused buffer shows, though
    /// the repository lists another file first.
    #[test]
    fn an_open_lands_on_the_conflicted_file_the_focused_buffer_shows() {
        let mut h = Stoat::test();
        seed_two_files(&mut h);
        let path = h.stoat.active_workspace().git_root.join("b.txt");
        h.fake_fs().insert_file(&path, MARKER);
        h.open_file(&path);
        dispatch_conflict(&mut h);

        let shown = h
            .stoat
            .active_workspace()
            .conflict
            .as_ref()
            .map(|session| (session.current, session.file.path.clone()));
        assert_eq!(shown, Some((1, path)));
    }

    /// A file's open count holds against its center buffer's version. A held
    /// count answers in place of a count while the version stands, and a pick
    /// that moves the text counts the file again.
    #[test]
    fn an_open_count_holds_until_the_text_moves() {
        let count = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            let file = &ws.conflict.as_ref().expect("session open").file;
            super::open_chunks(file, &ws.buffers)
        };
        let mut h = Stoat::test();
        open(&mut h);
        let counted = count(&h);
        {
            let ws = h.stoat.active_workspace();
            let file = &ws.conflict.as_ref().expect("session open").file;
            let (version, _) = file.open_chunks.get().expect("the count holds");
            file.open_chunks.set(Some((version, 7)));
        }
        let held = count(&h);
        pick(&mut h, &ConflictPickOurs);

        assert_eq!((counted, held, count(&h)), (1, 7, 0));
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

    /// The index records only that an applied file is staged, so the
    /// workspace notes that the view resolved it, which the git status panel
    /// shows.
    #[test]
    fn apply_notes_the_file_resolved_for_the_status_panel() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        seed_conflict(&mut h);
        dispatch_conflict(&mut h);

        pick(&mut h, &ConflictPickOurs);
        pick(&mut h, &ConflictApply);

        assert_eq!(
            h.stoat.active_workspace().resolved_conflicts,
            HashMap::from([(git_root.join("f.txt"), Note::Resolved)]),
        );
    }

    /// The index write waits its turn in the git queue, so the view stays on
    /// the file until the resolution lands.
    #[test]
    fn apply_lands_the_resolution_from_the_git_queue() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        seed_conflict(&mut h);
        dispatch_conflict(&mut h);
        let path = git_root.join("f.txt");
        pick(&mut h, &ConflictPickOurs);

        crate::action_handlers::dispatch(&mut h.stoat, &ConflictApply);
        assert_eq!(
            h.stoat.current_view(),
            Some("conflict"),
            "the press leaves the view on the file",
        );

        h.settle();
        assert_eq!(
            (
                h.stoat.current_view(),
                h.fake_git().resolved_paths(&git_root)
            ),
            (Some("file"), vec![path]),
            "the landed resolution closed the view",
        );
    }

    #[test]
    fn apply_writes_a_crlf_conflict_back_as_crlf() {
        // The center buffer is built out of the index stages rather than read
        // from disk, so it needs the same normalization an opened file gets.
        // Left alone the carriage returns become marker-block content, and the
        // resolved file comes back with them doubled.
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root.clone()).conflicted_file(
            "f.txt",
            Some("base\r\n"),
            Some("ours\r\n"),
            Some("theirs\r\n"),
        );
        dispatch_conflict(&mut h);
        let path = git_root.join("f.txt");

        assert!(
            !center_text(&h).contains('\r'),
            "the center buffer must hold no carriage returns"
        );

        pick(&mut h, &ConflictPickOurs);
        pick(&mut h, &ConflictApply);

        let mut written = Vec::new();
        h.stoat
            .fs_host
            .read(&path, &mut written)
            .expect("resolved file written");
        assert_eq!(String::from_utf8(written).unwrap(), "ours\r\n");
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
        let mut h = cell_harness(150, 20);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("start\nb\nc\nend\n"),
            Some("start\nO\nend\n"),
            Some("start\nT1\nT2\nend\n"),
        );

        dispatch_conflict(&mut h);
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
        let mut h = cell_harness(150, 20);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "src/f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );

        dispatch_conflict(&mut h);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        h.assert_snapshot("conflict_view_three_columns");
    }

    /// Under stoatty the view paints as the MergeGrid. The cells hold its
    /// header row, its tints, and its text, and the scene holds the rest.
    #[test]
    fn snapshot_conflict_view_merge_grid() {
        let mut h = TestHarness::with_size(150, 20);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "src/f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );

        dispatch_conflict(&mut h);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        h.assert_snapshot("conflict_view_merge_grid");
    }

    #[test]
    fn snapshot_conflict_view_narrow_drops_side_gutters() {
        let mut h = cell_harness(90, 16);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\n"),
            Some("a\nB\nc\n"),
            Some("a\nX\nc\n"),
        );

        dispatch_conflict(&mut h);

        assert_eq!(h.stoat.current_view(), Some("conflict"));
        h.assert_snapshot("conflict_view_narrow");
    }

    #[test]
    fn labels_name_the_head_branch_and_the_merge_head() {
        let merge = |head_name: Option<&str>| MergeState {
            head_sha: "f00dfeed1234".to_string(),
            head_name: head_name.map(String::from),
            message: String::new(),
            conflicted: 1,
        };
        let labels = |head: Option<&str>, merge: Option<MergeState>| {
            let labels = ConflictLabels::new(head.map(String::from), merge.as_ref());
            [
                labels.ours_label(),
                labels.theirs_label(),
                labels.ours_short().to_string(),
                labels.theirs_short().to_string(),
            ]
        };

        assert_eq!(
            [
                labels(Some("main"), Some(merge(Some("feature")))),
                labels(Some("main"), Some(merge(None))),
                labels(None, Some(merge(None))),
                labels(Some("main"), None),
            ],
            [
                [
                    "HEAD \u{b7} main",
                    "feature \u{b7} f00dfee",
                    "main",
                    "feature"
                ],
                ["HEAD \u{b7} main", "f00dfee", "main", "f00dfee"],
                ["HEAD", "f00dfee", "HEAD", "f00dfee"],
                ["HEAD \u{b7} main", "theirs", "main", "theirs"],
            ]
            .map(|labels| labels.map(String::from)),
            "a merge head's branch leads its short sha, a merge head no branch names \
             reads its sha alone, a detached HEAD reads HEAD, and conflicts with no \
             merge read theirs",
        );
    }

    #[test]
    fn a_stepped_to_file_shows_the_labels_the_open_read() {
        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git()
            .add_repo(git_root)
            .set_head_branch("main")
            .merge_in_progress("f00dfeed", Some("feature"), "Merge branch 'feature'")
            .conflicted_file("a.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"))
            .conflicted_file("b.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"));
        dispatch_conflict(&mut h);
        pick(&mut h, &ConflictNextFile);

        let labels = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .and_then(|editor| editor.conflict_view.as_ref())
            .map(|state| (state.labels.ours_label(), state.labels.theirs_label()));
        assert_eq!(
            (current_file(&h), labels),
            (
                1,
                Some((
                    "HEAD \u{b7} main".to_string(),
                    "feature \u{b7} f00dfee".to_string()
                ))
            ),
        );
    }

    #[test]
    fn the_position_names_the_operation_and_the_chunk_at_the_cursor() {
        let position = |h: &TestHarness| {
            super::conflict_position(h.stoat.active_workspace())
                .map(|at| (at.operation, at.chunk_at, at.chunk_total))
        };
        let mut h = Stoat::test();
        seed_two_chunks(&mut h);
        let closed = position(&h);
        dispatch_conflict(&mut h);
        let first = position(&h);
        // The first chunk's region ends where the line after its marker block
        // starts.
        h.stoat
            .collapse_focused_cursor_to("a\n<<<<<<< ours\nB\n=======\nX\n>>>>>>> theirs\n".len());
        let after_first = position(&h);
        pick(&mut h, &ConflictNextChunk);
        let second = position(&h);
        h.stoat.active_workspace_mut().rebase_active = Some(ActiveRebase::new(
            RebaseState::new("/repo".into(), "c1".into(), Vec::new()),
            None,
        ));
        let rebasing = position(&h);

        let mut merging = Stoat::test();
        let git_root = merging.stoat.active_workspace().git_root.clone();
        merging
            .fake_git()
            .add_repo(git_root)
            .merge_in_progress("f00dfeed", Some("feature"), "Merge branch 'feature'")
            .conflicted_file("f.txt", Some("base\n"), Some("ours\n"), Some("theirs\n"));
        dispatch_conflict(&mut merging);

        assert_eq!(
            [
                closed,
                first,
                after_first,
                second,
                rebasing,
                position(&merging)
            ],
            [
                None,
                Some(("conflict", Some(1), 2)),
                Some(("conflict", None, 2)),
                Some(("conflict", Some(2), 2)),
                Some(("rebase", Some(2), 2)),
                Some(("merge", Some(1), 1)),
            ],
            "the cursor's chunk counts from one, and the line after a chunk names \
             none",
        );
    }

    /// The position counts the session's files and the chunks still on their
    /// markers in every file. Three files hold one chunk each, a pick in the
    /// first leaves the other two open, and a step names the second file and
    /// its open chunk.
    #[test]
    fn the_position_counts_the_files_and_the_open_chunks_of_the_session() {
        let position = |h: &TestHarness| {
            super::conflict_position(h.stoat.active_workspace()).map(|at| {
                (
                    at.file_at,
                    at.file_total,
                    at.file_open,
                    at.chunks_open,
                    at.files_open,
                )
            })
        };
        let mut h = Stoat::test();
        seed_three_files(&mut h);
        dispatch_conflict(&mut h);
        let opened = position(&h);
        pick(&mut h, &ConflictPickOurs);
        let picked = position(&h);
        pick(&mut h, &ConflictNextFile);

        assert_eq!(
            [opened, picked, position(&h)],
            [
                Some((1, 3, 1, 3, 3)),
                Some((1, 3, 0, 2, 2)),
                Some((2, 3, 1, 2, 2)),
            ],
        );
    }
}
