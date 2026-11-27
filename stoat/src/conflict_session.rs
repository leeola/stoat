use crate::{
    buffer_registry::BufferRegistry,
    display_map::DisplaySnapshot,
    editor_state::EditorId,
    host::git::{GitRepo, MergeState},
    merge_view::{AlignRow, ChunkState, MergeDoc, RowPick},
    multi_buffer::{MultiBuffer, MultiBufferSnapshot},
    render::conflict_view::ConflictHover,
    workspace::{diff, Workspace},
};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use stoat_action::ActionKind;
use stoat_text::{Anchor, BufferId, Point};

/// A live three-way conflict resolution across a repository's conflicted files.
///
/// Owns the swapped-in center editor and the original editor displaced from the
/// pane, so closing the view restores the plain file. Held as
/// [`crate::workspace::Workspace::conflict`] while the `:conflict` view is open.
pub struct ConflictSession {
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
    pub current: usize,
    /// Resolve state of the file currently shown.
    pub file: FileResolveState,
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
    /// The chunk whose chip shows its three pick buttons, until a pick, a
    /// press elsewhere, or a key closes them.
    ///
    /// The shown file's [`ConflictViewState::chip_menu`] mirrors it, so the
    /// painter and a pool page draw the buttons.
    pub chip_menu: Option<usize>,
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
pub struct FileResolveState {
    /// Absolute path of the conflicted file.
    pub path: PathBuf,
    /// The three-way merge of the file's index stages.
    pub(crate) doc: MergeDoc,
    /// Per-chunk row picks, one entry per chunk sized to its row count.
    pub(crate) picks: Vec<Vec<RowPick>>,
    /// Start and end anchors of each chunk's region in the center buffer.
    pub(crate) chunk_anchors: Vec<(Anchor, Anchor)>,
    /// The center (result) buffer holding the editable merged text.
    pub buffer_id: BufferId,
    /// The center editor swapped into the pane.
    pub editor_id: EditorId,
    /// The chunks still on their markers, with the buffer version they were
    /// counted at, so a frame counts a file again only after its text moved.
    pub open_chunks: Cell<Option<(u64, usize)>>,
}

/// Per-editor render cache for the three-column conflict view.
///
/// Cloned from the session onto the swapped-in editor at open, so the renderer,
/// which receives the focused editor alone, builds its column display list
/// from `doc` each frame. Refreshed alongside the session when a pick
/// reassembles a region.
#[derive(Clone)]
pub struct ConflictViewState {
    pub(crate) doc: MergeDoc,
    /// Start/end anchors of each chunk's center region, so the renderer can
    /// resolve the current row span of a chunk (which shrinks when a pick
    /// reassembles the region) and align the side columns to it.
    pub(crate) chunk_anchors: Vec<(Anchor, Anchor)>,
    /// Per-chunk row picks mirrored from the session, so the renderer can
    /// derive each chunk's resolution state against its live region text and
    /// paint the matching gutter glyph.
    pub(crate) picks: Vec<Vec<RowPick>>,
    /// The file's path from the repository's root, which the File Header
    /// names.
    pub(crate) rel_path: String,
    /// The lines theirs adds and removes against ours, which the File Header
    /// counts.
    ///
    /// Counted once, since the merge document never changes and the painter
    /// runs every frame.
    pub(crate) line_counts: (u32, u32),
    /// The names the MergeGrid header gives the two sides.
    pub labels: ConflictLabels,
    /// The chunk whose chip shows its pick buttons, as
    /// [`ConflictSession::chip_menu`] holds it while this file is shown.
    pub chip_menu: Option<usize>,
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
pub struct ConflictDerived {
    buffer_version: u64,
    pub states: Vec<ChunkState>,
    pub plan: Vec<AlignRow>,
}

impl ConflictViewState {
    pub(crate) fn new(
        doc: MergeDoc,
        chunk_anchors: Vec<(Anchor, Anchor)>,
        picks: Vec<Vec<RowPick>>,
        rel_path: String,
        labels: ConflictLabels,
    ) -> Self {
        Self {
            line_counts: doc.line_counts(),
            doc,
            chunk_anchors,
            picks,
            rel_path,
            labels,
            chip_menu: None,
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
    pub fn derived(&mut self, snapshot: &DisplaySnapshot) -> (&MergeDoc, &ConflictDerived) {
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
    pub fn invalidate_derived(&mut self) {
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
pub struct ConflictLabels {
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
    pub fn ours_label(&self) -> String {
        match &self.ours_branch {
            Some(branch) => format!("HEAD \u{b7} {branch}"),
            None => self.ours_short().to_string(),
        }
    }

    /// Theirs as the MergeGrid's header names it, `{branch} · {sha}`, or the
    /// short form when no branch is at the merge head or no merge is in
    /// progress.
    pub fn theirs_label(&self) -> String {
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
pub struct ConflictPosition {
    /// `merge` during a merge, `rebase` during a rebase, and `conflict` for
    /// conflicts with neither behind them.
    pub operation: &'static str,
    /// The 1-based chunk the cursor stands in, or `None` outside every chunk.
    pub chunk_at: Option<usize>,
    pub chunk_total: usize,
    /// The 1-based place of the shown file in the session's file list.
    pub file_at: usize,
    /// The conflicted files the session lists.
    pub file_total: usize,
    /// The shown file's chunks still on their markers.
    pub file_open: usize,
    /// The chunks still on their markers in every file of the session.
    pub chunks_open: usize,
    /// The files of the session that hold a chunk still on its markers.
    pub files_open: usize,
}

/// What a frame hands the conflict view's painter beside the editor's own
/// state.
///
/// The painter receives the editor alone, so the frame writes the session's
/// position on the shown file's center editor before the panes paint.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConflictFrame {
    /// The position the frame read, `None` when the focused pane shows no
    /// conflict view.
    pub(crate) position: Option<ConflictPosition>,
    /// The open chunk's side the pointer rests over, as the last motion event
    /// found it.
    pub hover: Option<ConflictHover>,
}

/// The focused conflict view's [`ConflictPosition`], or `None` when the focused
/// pane shows no conflict view.
///
/// The cursor is the newest selection's start, the place the chunk keys read,
/// so the chunk the position names is the chunk a pick acts on. The open
/// counts read each file's held count, as [`open_chunks`] keeps it.
pub fn conflict_position(ws: &Workspace) -> Option<ConflictPosition> {
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
pub fn open_chunks(file: &FileResolveState, buffers: &BufferRegistry) -> usize {
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

/// The display rows of each open chunk's center region in the merged column
/// that `snapshot` shows, in chunk order.
///
/// A chunk is open while its region still holds its conflict markers, as
/// [`open_chunks`] counts it, so a pick or a hand edit takes its rows off. The
/// conflict view's strip marks these rows.
pub(crate) fn open_rows(state: &ConflictViewState, snapshot: &DisplaySnapshot) -> Vec<Range<u32>> {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let display_row = |anchor: &Anchor| {
        let row = rope
            .offset_to_point(buffer_snapshot.resolve_anchor(anchor))
            .row;
        snapshot.buffer_to_display(Point::new(row, 0)).row
    };
    let states = chunk_states(snapshot, &state.doc, &state.chunk_anchors, &state.picks);

    state
        .chunk_anchors
        .iter()
        .zip(states)
        .filter(|(_, chunk)| *chunk == ChunkState::Unresolved)
        .map(|((start, end), _)| display_row(start)..display_row(end))
        .collect()
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
    use crate::host::git::MergeState;

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
}
