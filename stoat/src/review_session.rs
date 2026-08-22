use crate::{
    diff_map::BaseHighlights,
    editor_state::EditorId,
    host::WatchToken,
    review::{
        extract_review_hunks_changeset, line_byte_offsets, split_lines, ReviewFileInput,
        ReviewHunk, ReviewRow, ReviewSide,
    },
};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_language::{structural_diff::TreeCache, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReviewChunkId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkStatus {
    Pending,
    Staged,
    Unstaged,
    Skipped,
}

impl ChunkStatus {
    pub(crate) fn is_decided(self) -> bool {
        matches!(
            self,
            ChunkStatus::Staged | ChunkStatus::Unstaged | ChunkStatus::Skipped
        )
    }
}

/// Provenance of the content under review.
///
/// - [`ReviewSource::WorkingTree`]: git index vs working tree of `workdir`.
/// - [`ReviewSource::Commit`]: commit tree vs its parent (empty tree for a root commit).
/// - [`ReviewSource::CommitRange`]: `from..=to`, diff between the trees at the two commits,
///   inclusive of `to`.
/// - [`ReviewSource::AgentEdits`]: in-memory edit proposals; no repo required.
/// - [`ReviewSource::InMemory`]: test-only placeholder; not rescannable.
#[derive(Clone, Debug)]
pub(crate) enum ReviewSource {
    WorkingTree {
        workdir: PathBuf,
    },
    Commit {
        workdir: PathBuf,
        sha: String,
    },
    CommitRange {
        workdir: PathBuf,
        from: String,
        to: String,
    },
    AgentEdits {
        edits: Arc<Vec<AgentEditProposal>>,
    },
    #[allow(dead_code)]
    InMemory {
        files: Arc<Vec<InMemoryFile>>,
    },
}

impl ReviewSource {
    /// The working directory this source diffs against, for the git-backed
    /// variants. `None` for the non-git sources (agent edits, in-memory), which
    /// have no repository to re-decide against.
    pub(crate) fn workdir(&self) -> Option<&Path> {
        match self {
            ReviewSource::WorkingTree { workdir }
            | ReviewSource::Commit { workdir, .. }
            | ReviewSource::CommitRange { workdir, .. } => Some(workdir),
            ReviewSource::AgentEdits { .. } | ReviewSource::InMemory { .. } => None,
        }
    }
}

/// Test-only / future-facing carrier for agent-proposed edits. Kept as a
/// concrete type rather than an opaque placeholder so the variant signature
/// does not churn when the real agent bridge lands.
#[derive(Clone, Debug)]
pub(crate) struct AgentEditProposal {
    pub path: PathBuf,
    pub base_text: Arc<String>,
    pub proposed_text: Arc<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct InMemoryFile {
    pub path: PathBuf,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewChunk {
    #[allow(dead_code)]
    pub id: ReviewChunkId,
    pub file_index: usize,
    pub chunk_index_in_file: usize,
    pub hunk: ReviewHunk,
    /// 0-based half-open row range in the buffer (RHS) text. Empty for
    /// pure-deletion chunks; callers scrolling to a chunk should fall
    /// back to `base_line_range` in that case.
    pub buffer_line_range: Range<u32>,
    /// 0-based half-open row range in the base (LHS) text.
    pub base_line_range: Range<u32>,
    #[allow(dead_code)]
    pub buffer_byte_range: Range<usize>,
    pub base_byte_range: Range<usize>,
    pub status: ChunkStatus,
}

#[derive(Clone)]
pub(crate) struct ReviewFile {
    pub path: PathBuf,
    pub rel_path: String,
    pub language: Option<Arc<Language>>,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
    pub chunks: Vec<ReviewChunkId>,
    /// Tree-sitter spans for each side, per 0-based line, so a preview paints
    /// the same token colors the editor does.
    ///
    /// `None` until a preview build attaches them, and left `None` for a file
    /// with no language or when syntax highlighting is off. The paint falls
    /// back to untokenized text either way, which is what a syntax-off editor
    /// shows. Baked against the theme in force at build, so a theme switch
    /// drops the sessions holding them.
    pub base_highlights: Option<Arc<BaseHighlights>>,
    pub buffer_highlights: Option<Arc<BaseHighlights>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReviewCursor {
    pub current: Option<ReviewChunkId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewProgress {
    pub staged: usize,
    pub unstaged: usize,
    pub skipped: usize,
    pub pending: usize,
    pub total: usize,
    /// 1-based index of `cursor.current` within the flattened order, or
    /// `None` if the cursor has not settled on a chunk yet.
    pub current_index: Option<usize>,
}

impl ReviewProgress {
    /// True when the session has at least one chunk and every chunk
    /// has been decided (staged, unstaged, or skipped).
    pub(crate) fn is_complete(&self) -> bool {
        self.total > 0 && self.pending == 0
    }
}

/// UI-facing cache derived from a [`ReviewSession`]. Attached to an editor
/// so `render_review` can paint without walking the session on every frame,
/// and so navigation handlers can map a chunk id to a display row.
#[derive(Clone, Debug)]
pub(crate) struct ReviewViewState {
    /// Every file's full before/after source as aligned rows, in visit order.
    /// One row per placeholder-buffer line. Chunk rows carry the diff. The
    /// unchanged regions between and around them are filled with derived
    /// Context rows so the whole file renders, not just the changed excerpts.
    pub rows: Vec<ReviewRow>,
    /// Half-open display-row range each chunk occupies, ordered by row and
    /// non-overlapping. Rows falling in the gaps between ranges belong to no
    /// chunk. Used for row-to-chunk lookup and chunk-to-scroll-row lookup.
    pub chunk_row_ranges: Vec<(ReviewChunkId, Range<u32>)>,
    /// Status of each chunk, indexed parallel to `chunk_row_ranges`. Kept
    /// here so `render_review` can paint gutter glyphs without holding a
    /// reference to the session.
    pub chunk_statuses: Vec<ChunkStatus>,
    /// Chunk currently under the review cursor, if any. Rendered with an
    /// additional highlight so the user can tell which chunk their
    /// navigation keys will act on.
    pub current_chunk: Option<ReviewChunkId>,
    /// Session version this cache was built from.
    pub session_version: u64,
    /// Whether the diff view auto-refreshes on repo changes, mirroring
    /// `review_follow`. Only affects the empty-state message: a clean tree that
    /// is watched invites the user to expect live updates, one that is not does
    /// not. Set by the render editor build, which has the settings.
    pub watching: bool,
}

impl ReviewViewState {
    pub(crate) fn from_session(session: &ReviewSession) -> Self {
        let mut rows: Vec<ReviewRow> = Vec::new();
        let mut chunk_row_ranges: Vec<(ReviewChunkId, Range<u32>)> = Vec::new();
        let mut chunk_statuses: Vec<ChunkStatus> = Vec::new();

        for file in &session.doc.files {
            let base_lines = split_lines(&file.base_text);
            let buffer_lines = split_lines(&file.buffer_text);
            let mut base_cursor: u32 = 0;
            let mut buffer_cursor: u32 = 0;

            for chunk_id in &file.chunks {
                let Some(chunk) = session.doc.chunks.get(chunk_id) else {
                    continue;
                };
                let (base_disp, buffer_disp) = hunk_display_ranges(&chunk.hunk);
                let base_start = base_disp.as_ref().map_or(base_cursor, |r| r.start);
                let buffer_start = buffer_disp.as_ref().map_or(buffer_cursor, |r| r.start);

                emit_context_gap(
                    &mut rows,
                    &base_lines,
                    &buffer_lines,
                    base_cursor..base_start,
                    buffer_cursor..buffer_start,
                );

                let start = rows.len() as u32;
                rows.extend(chunk.hunk.rows.iter().cloned());
                chunk_row_ranges.push((*chunk_id, start..rows.len() as u32));
                chunk_statuses.push(chunk.status);

                base_cursor = base_disp.map_or(base_cursor, |r| r.end);
                buffer_cursor = buffer_disp.map_or(buffer_cursor, |r| r.end);
            }

            emit_context_gap(
                &mut rows,
                &base_lines,
                &buffer_lines,
                base_cursor..base_lines.len() as u32,
                buffer_cursor..buffer_lines.len() as u32,
            );
        }

        Self {
            rows,
            chunk_row_ranges,
            chunk_statuses,
            current_chunk: session.cursor.current,
            session_version: session.version,
            watching: true,
        }
    }

    /// Sync the status cache and cursor from the session without rebuilding
    /// row data. Cheaper than `from_session` and the right call when only
    /// the cursor or a chunk's status has changed.
    pub(crate) fn refresh_from_session(&mut self, session: &ReviewSession) {
        if self.session_version == session.version {
            return;
        }
        self.chunk_statuses.clear();
        self.chunk_statuses.reserve(self.chunk_row_ranges.len());
        for (id, _) in &self.chunk_row_ranges {
            let status = session
                .doc
                .chunks
                .get(id)
                .map(|c| c.status)
                .unwrap_or(ChunkStatus::Pending);
            self.chunk_statuses.push(status);
        }
        self.current_chunk = session.cursor.current;
        self.session_version = session.version;
    }

    /// Returns the (chunk_id, status) for the given display row, or `None` when
    /// the row falls in an unchanged gap between chunks and so belongs to none.
    pub(crate) fn chunk_and_status_at_row(&self, row: u32) -> Option<(ReviewChunkId, ChunkStatus)> {
        let idx = self
            .chunk_row_ranges
            .partition_point(|(_, range)| range.start <= row)
            .checked_sub(1)?;
        let (id, range) = &self.chunk_row_ranges[idx];
        if row >= range.end {
            return None;
        }
        let status = self.chunk_statuses.get(idx).copied()?;
        Some((*id, status))
    }

    /// Returns the first display row of the given chunk, or `None` if the
    /// chunk is not represented in this view.
    pub(crate) fn row_of_chunk(&self, id: ReviewChunkId) -> Option<u32> {
        self.chunk_row_ranges
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, r)| r.start)
    }
}

// FIXME: Per-chunk Staged/Unstaged/Skipped status not persisted across
// workspace save/load. [`ReviewChunkId`] is allocated fresh per session, so we
// cannot simply serialize the HashMap keyed on it. Resolution: assign each
// chunk a stable fingerprint (e.g. blake3 of pre+post content + base line
// range) at chunk-creation time, persist a
// `HashMap<ChunkFingerprint, ChunkStatus>`, and re-key on load. Chunks whose
// fingerprint no longer matches (underlying file changed externally) degrade
// to `Pending`.
/// A built diff, as the files it covers and the chunks they are cut into.
///
/// This is everything a reader of a diff needs and nothing a reviewer of one
/// does. The commit picker and the commits view show a commit's diff without
/// staging anything in it, so they hold one of these rather than a whole
/// [`ReviewSession`], whose cursor, staged status, view editor, watch tokens,
/// and version counter answer questions a preview never asks.
///
/// Keeping the two apart is what lets a preview outlive the review machinery.
#[derive(Default)]
pub(crate) struct DiffDocument {
    pub files: Vec<ReviewFile>,
    pub chunks: HashMap<ReviewChunkId, ReviewChunk>,
}

pub(crate) struct ReviewSession {
    pub source: ReviewSource,
    /// True when the `:diff` command owns this session, so a git refresh
    /// re-decides its content from the working tree rather than rescanning the
    /// displayed source as-is. It is what keeps a rebase-fallback [`Commit`]
    /// session (otherwise frozen) following each rebase step.
    ///
    /// [`Commit`]: ReviewSource::Commit
    pub auto_source: bool,
    /// The built diff itself, which a preview reads on its own.
    pub doc: DiffDocument,
    pub order: Vec<ReviewChunkId>,
    pub cursor: ReviewCursor,
    pub view_editor: Option<EditorId>,
    /// True while the diff is toggled off. The pane then shows a plain
    /// file editor and [`Self::view_editor`] is parked off-screen but
    /// kept alive so a toggle back is instant. The editor GC skips the
    /// parked editor for exactly this reason.
    pub toggled_off: bool,
    /// Review scroll row captured at toggle-off, used to reposition the
    /// diff on toggle-back when the file cursor cannot be mapped to a
    /// chunk (e.g. the pane now shows a file not in this session).
    pub stashed_display_row: Option<u32>,
    /// Bumped on any mutation so editor-level caches can detect staleness.
    pub version: u64,
    /// Base-side parse trees retained across this session's diffs.
    ///
    /// A refresh re-diffs the session's files against bases that have usually
    /// not moved, so the parses carry over while the buffers reparse. Scoped
    /// to the session because that is the span over which a base repeats.
    tree_cache: TreeCache,
    /// Filesystem-watch tokens covering each path in [`Self::files`].
    /// Populated only when [`Self::source`] is
    /// [`ReviewSource::WorkingTree`]; other sources skip watching
    /// because their content is not on disk.
    pub watch_tokens: Vec<WatchToken>,
    next_id: u32,
}

impl ReviewSession {
    pub(crate) fn new(source: ReviewSource) -> Self {
        Self {
            source,
            auto_source: false,
            doc: DiffDocument::default(),
            order: Vec::new(),
            cursor: ReviewCursor::default(),
            view_editor: None,
            toggled_off: false,
            stashed_display_row: None,
            version: 0,
            tree_cache: TreeCache::default(),
            watch_tokens: Vec::new(),
            next_id: 0,
        }
    }

    /// Single-file convenience wrapper around [`Self::add_files`].
    /// Test-only because production callers always have multiple files
    /// in hand and must batch them through `add_files` for cross-file
    /// move detection to fire.
    #[cfg(test)]
    pub(crate) fn add_file(
        &mut self,
        path: PathBuf,
        rel_path: String,
        language: Option<Arc<Language>>,
        base_text: Arc<String>,
        buffer_text: Arc<String>,
    ) -> Vec<ReviewChunkId> {
        let mut result = self.add_files(vec![ReviewFileInput {
            path,
            rel_path,
            language,
            base_text,
            buffer_text,
        }]);
        result.pop().unwrap_or_default()
    }

    /// Add one or more files to the session in a single cross-file
    /// structural-diff pass. Returns one chunk-id list per input in
    /// input order. Files that produce no hunks are still recorded
    /// (with an empty chunk list) so that subsequent file indices
    /// stay stable.
    pub(crate) fn add_files(&mut self, files: Vec<ReviewFileInput>) -> Vec<Vec<ReviewChunkId>> {
        let hunks_per_file = {
            let memo = self.tree_cache.clone();
            extract_review_hunks_changeset(&files, 3, Some(&memo))
        };
        self.add_files_with_hunks(files, hunks_per_file)
    }

    /// Add files that already have their hunks computed, skipping the
    /// changeset diff [`add_files`] runs. The streaming review scan uses
    /// this to reuse hunks it produced from prepared diffs, so a scanned
    /// file is never diffed a second time.
    pub(crate) fn add_files_with_hunks(
        &mut self,
        files: Vec<ReviewFileInput>,
        hunks_per_file: Vec<Vec<ReviewHunk>>,
    ) -> Vec<Vec<ReviewChunkId>> {
        let all_chunk_ids: Vec<Vec<ReviewChunkId>> = files
            .into_iter()
            .zip(hunks_per_file)
            .map(|(file, hunks)| self.push_file_with_hunks(file, hunks))
            .collect();

        if self.cursor.current.is_none() {
            self.cursor.current = self.order.first().copied();
        }

        self.version += 1;
        all_chunk_ids
    }

    /// Append one already-diffed file's chunks with a stable file index,
    /// returning the ids allocated for its hunks.
    ///
    /// Leaves the cursor and version untouched so a caller adding a batch of
    /// files sets the cursor and bumps the version once at the end rather than
    /// per file.
    fn push_file_with_hunks(
        &mut self,
        file: ReviewFileInput,
        hunks: Vec<ReviewHunk>,
    ) -> Vec<ReviewChunkId> {
        let file_index = self.doc.files.len();

        let base_offsets = line_byte_offsets(&split_lines(&file.base_text));
        let buffer_offsets = line_byte_offsets(&split_lines(&file.buffer_text));

        let mut chunk_ids: Vec<ReviewChunkId> = Vec::with_capacity(hunks.len());
        for (chunk_index_in_file, hunk) in hunks.into_iter().enumerate() {
            let id = self.alloc_id();
            let (base_line_range, buffer_line_range) = hunk_line_ranges(&hunk);
            let base_byte_range = lines_to_bytes(&base_offsets, &base_line_range);
            let buffer_byte_range = lines_to_bytes(&buffer_offsets, &buffer_line_range);

            self.doc.chunks.insert(
                id,
                ReviewChunk {
                    id,
                    file_index,
                    chunk_index_in_file,
                    hunk,
                    buffer_line_range,
                    base_line_range,
                    buffer_byte_range,
                    base_byte_range,
                    status: ChunkStatus::Pending,
                },
            );
            self.order.push(id);
            chunk_ids.push(id);
        }

        self.doc.files.push(ReviewFile {
            path: file.path,
            rel_path: file.rel_path,
            language: file.language,
            base_text: file.base_text,
            buffer_text: file.buffer_text,
            chunks: chunk_ids.clone(),
            base_highlights: None,
            buffer_highlights: None,
        });

        chunk_ids
    }

    #[allow(dead_code)]
    pub(crate) fn chunk(&self, id: ReviewChunkId) -> Option<&ReviewChunk> {
        self.doc.chunks.get(&id)
    }

    /// Resolve an in-buffer byte offset to the chunk that should
    /// receive cursor focus. A chunk whose `buffer_byte_range` covers
    /// the byte wins outright; otherwise the first chunk starting at
    /// or after the byte; otherwise the file's last chunk so callers
    /// past every existing hunk still get a navigation target.
    /// Returns `None` only when `file_index` is out of range or the
    /// file has no chunks.
    pub(crate) fn chunk_containing_buffer_byte(
        &self,
        file_index: usize,
        buffer_byte: usize,
    ) -> Option<ReviewChunkId> {
        let file = self.doc.files.get(file_index)?;
        let mut last: Option<ReviewChunkId> = None;
        for id in &file.chunks {
            let chunk = self.doc.chunks.get(id)?;
            if chunk.buffer_byte_range.contains(&buffer_byte) {
                return Some(*id);
            }
            if chunk.buffer_byte_range.start >= buffer_byte {
                return Some(*id);
            }
            last = Some(*id);
        }
        last
    }

    #[allow(dead_code)]
    pub(crate) fn current(&self) -> Option<&ReviewChunk> {
        self.cursor.current.and_then(|id| self.doc.chunks.get(&id))
    }

    /// Advance the cursor to the next chunk. Clamps at the last chunk and
    /// returns `None` when already there (callers may surface this as an
    /// "end of review" signal).
    pub(crate) fn next(&mut self) -> Option<ReviewChunkId> {
        let idx = self.cursor_order_index();
        let next_idx = match idx {
            None if !self.order.is_empty() => 0,
            Some(i) if i + 1 < self.order.len() => i + 1,
            _ => return None,
        };
        let id = self.order[next_idx];
        self.cursor.current = Some(id);
        self.version += 1;
        Some(id)
    }

    pub(crate) fn prev(&mut self) -> Option<ReviewChunkId> {
        let idx = self.cursor_order_index()?;
        if idx == 0 {
            return None;
        }
        let id = self.order[idx - 1];
        self.cursor.current = Some(id);
        self.version += 1;
        Some(id)
    }

    pub(crate) fn set_status(&mut self, id: ReviewChunkId, status: ChunkStatus) {
        if let Some(chunk) = self.doc.chunks.get_mut(&id) {
            chunk.status = status;
            self.version += 1;
        }
    }

    /// Toggle between `Staged` and `Unstaged` for the given chunk. Chunks
    /// currently in `Pending` or `Skipped` flip to `Staged`, giving users
    /// a one-key path from "not looked at" into the accept lane.
    pub(crate) fn toggle_stage(&mut self, id: ReviewChunkId) {
        if let Some(chunk) = self.doc.chunks.get_mut(&id) {
            chunk.status = match chunk.status {
                ChunkStatus::Staged => ChunkStatus::Unstaged,
                ChunkStatus::Unstaged | ChunkStatus::Pending | ChunkStatus::Skipped => {
                    ChunkStatus::Staged
                },
            };
            self.version += 1;
        }
    }

    pub(crate) fn progress(&self) -> ReviewProgress {
        let mut p = ReviewProgress {
            total: self.order.len(),
            current_index: self.cursor_order_index().map(|i| i + 1),
            ..Default::default()
        };
        for id in &self.order {
            if let Some(chunk) = self.doc.chunks.get(id) {
                match chunk.status {
                    ChunkStatus::Staged => p.staged += 1,
                    ChunkStatus::Unstaged => p.unstaged += 1,
                    ChunkStatus::Skipped => p.skipped += 1,
                    ChunkStatus::Pending => p.pending += 1,
                }
            }
        }
        p
    }

    pub(crate) fn is_complete(&self) -> bool {
        !self.order.is_empty()
            && self
                .order
                .iter()
                .filter_map(|id| self.doc.chunks.get(id))
                .all(|c| c.status.is_decided())
    }

    /// Lookup key for carrying status across a refresh. Combines file path,
    /// base line range, and a content hash of the base text for the chunk
    /// so that a chunk surviving a refresh keeps its decision, while a
    /// chunk whose underlying content moved or changed is treated as new.
    pub(crate) fn identity_key(&self, id: ReviewChunkId) -> Option<ChunkIdentity> {
        let chunk = self.doc.chunks.get(&id)?;
        let file = self.doc.files.get(chunk.file_index)?;
        let slice = file
            .base_text
            .get(chunk.base_byte_range.clone())
            .unwrap_or("");
        let mut hasher = DefaultHasher::new();
        slice.hash(&mut hasher);
        Some(ChunkIdentity {
            path: file.path.clone(),
            base_line_start: chunk.base_line_range.start,
            base_line_end: chunk.base_line_range.end,
            content_hash: hasher.finish(),
        })
    }

    fn alloc_id(&mut self) -> ReviewChunkId {
        let id = ReviewChunkId(self.next_id);
        self.next_id += 1;
        id
    }

    fn cursor_order_index(&self) -> Option<usize> {
        let current = self.cursor.current?;
        self.order.iter().position(|id| *id == current)
    }
}

/// Stable, refresh-friendly key for a chunk. See [`ReviewSession::identity_key`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ChunkIdentity {
    pub path: PathBuf,
    pub base_line_start: u32,
    pub base_line_end: u32,
    pub content_hash: u64,
}

fn lines_to_bytes(offsets: &[(usize, usize)], lines: &Range<u32>) -> Range<usize> {
    if lines.start >= lines.end || offsets.is_empty() {
        return 0..0;
    }
    let start_idx = lines.start as usize;
    let end_idx = (lines.end as usize).min(offsets.len());
    if start_idx >= offsets.len() {
        return 0..0;
    }
    let start = offsets[start_idx].0;
    let end = offsets[end_idx.saturating_sub(1)].1;
    start..end
}

/// Returns the (base, buffer) 0-based half-open line ranges covered by the
/// changed rows of the hunk. Context rows are excluded because a chunk is
/// addressed by its *change*, not its display extent.
fn hunk_line_ranges(hunk: &ReviewHunk) -> (Range<u32>, Range<u32>) {
    let mut base_min: Option<u32> = None;
    let mut base_max: Option<u32> = None;
    let mut buf_min: Option<u32> = None;
    let mut buf_max: Option<u32> = None;

    for row in &hunk.rows {
        if let ReviewRow::Changed { left, right } = row {
            if let Some(l) = left {
                let v = l.line_num.saturating_sub(1);
                base_min = Some(base_min.map_or(v, |m| m.min(v)));
                base_max = Some(base_max.map_or(v + 1, |m| m.max(v + 1)));
            }
            if let Some(r) = right {
                let v = r.line_num.saturating_sub(1);
                buf_min = Some(buf_min.map_or(v, |m| m.min(v)));
                buf_max = Some(buf_max.map_or(v + 1, |m| m.max(v + 1)));
            }
        }
    }

    let base = match (base_min, base_max) {
        (Some(s), Some(e)) => s..e,
        _ => 0..0,
    };
    let buffer = match (buf_min, buf_max) {
        (Some(s), Some(e)) => s..e,
        _ => 0..0,
    };
    (base, buffer)
}

/// The (base, buffer) 0-based half-open line ranges a hunk's full displayed
/// extent spans, context rows included.
///
/// Unlike [`hunk_line_ranges`], which reports only the changed extent, this
/// covers every row the hunk renders. `None` on a side the hunk never touches:
/// a wholly-added file has no base line on any row, a wholly-deleted file no
/// buffer line, so the caller leaves that side's walk cursor where it is and
/// emits no gap there.
fn hunk_display_ranges(hunk: &ReviewHunk) -> (Option<Range<u32>>, Option<Range<u32>>) {
    let mut base: Option<Range<u32>> = None;
    let mut buffer: Option<Range<u32>> = None;

    for row in &hunk.rows {
        let (left, right) = match row {
            ReviewRow::Context { left, right } => (Some(left), Some(right)),
            ReviewRow::Changed { left, right } => (left.as_ref(), right.as_ref()),
        };
        if let Some(l) = left {
            base = Some(extend_line_range(base, l.line_num.saturating_sub(1)));
        }
        if let Some(r) = right {
            buffer = Some(extend_line_range(buffer, r.line_num.saturating_sub(1)));
        }
    }

    (base, buffer)
}

fn extend_line_range(current: Option<Range<u32>>, line: u32) -> Range<u32> {
    match current {
        Some(r) => r.start.min(line)..r.end.max(line + 1),
        None => line..line + 1,
    }
}

/// Emit one Context row per unchanged line in the gap, pairing base line with
/// buffer line 1:1.
///
/// The two ranges cover the same unchanged region on each side, so they have
/// equal length in a well-formed diff. Zipping tolerates the degenerate
/// boundary case (one side empty) without emitting a half-populated row.
fn emit_context_gap(
    rows: &mut Vec<ReviewRow>,
    base_lines: &[&str],
    buffer_lines: &[&str],
    base_range: Range<u32>,
    buffer_range: Range<u32>,
) {
    for (b, r) in base_range.zip(buffer_range) {
        let (Some(base_line), Some(buffer_line)) =
            (base_lines.get(b as usize), buffer_lines.get(r as usize))
        else {
            continue;
        };
        rows.push(ReviewRow::Context {
            left: context_side(base_line, b + 1),
            right: context_side(buffer_line, r + 1),
        });
    }
}

fn context_side(text: &str, line_num: u32) -> ReviewSide {
    ReviewSide {
        text: text.to_string(),
        line_num,
        change_spans: Vec::new(),
        moved_spans: Vec::new(),
        move_provenance: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_session() -> ReviewSession {
        ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        })
    }

    fn add(
        session: &mut ReviewSession,
        path: &str,
        base: &str,
        buffer: &str,
    ) -> Vec<ReviewChunkId> {
        session.add_file(
            PathBuf::from(path),
            path.to_string(),
            None,
            Arc::new(base.to_string()),
            Arc::new(buffer.to_string()),
        )
    }

    #[test]
    fn empty_session_has_no_progress() {
        let s = in_memory_session();
        assert_eq!(s.progress(), ReviewProgress::default());
        assert!(!s.is_complete());
        assert!(s.current().is_none());
    }

    #[test]
    fn add_file_assigns_ids_and_cursor() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(ids.len(), 1);
        assert_eq!(s.cursor.current, Some(ids[0]));
        assert_eq!(s.order, ids);
        assert_eq!(s.doc.files.len(), 1);
        assert_eq!(s.doc.files[0].chunks, ids);
    }

    #[test]
    fn add_files_with_hunks_matches_add_files() {
        let files = || {
            vec![
                ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new("a\nb\nc\n".to_string()),
                    buffer_text: Arc::new("a\nB\nc\n".to_string()),
                },
                ReviewFileInput {
                    path: PathBuf::from("b.txt"),
                    rel_path: "b.txt".to_string(),
                    language: None,
                    base_text: Arc::new("x\ny\n".to_string()),
                    buffer_text: Arc::new("x\nY\n".to_string()),
                },
            ]
        };

        let mut via_add = in_memory_session();
        let ids_add = via_add.add_files(files());

        let mut via_hunks = in_memory_session();
        let file_set = files();
        let precomputed = extract_review_hunks_changeset(&file_set, 3, None);
        let ids_hunks = via_hunks.add_files_with_hunks(file_set, precomputed);

        assert_eq!(
            ids_add, ids_hunks,
            "prebuilt hunks yield the same chunk ids"
        );
        assert_eq!(via_add.order, via_hunks.order);
    }

    #[test]
    fn next_prev_clamp() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        assert_eq!(s.cursor.current, Some(ids[0]));

        assert_eq!(s.next(), Some(ids[1]));
        assert_eq!(s.next(), None);
        assert_eq!(s.cursor.current, Some(ids[1]));

        assert_eq!(s.prev(), Some(ids[0]));
        assert_eq!(s.prev(), None);
        assert_eq!(s.cursor.current, Some(ids[0]));
    }

    #[test]
    fn toggle_stage_cycles_binary() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\n", "a\nB\n");
        let id = ids[0];
        assert_eq!(s.doc.chunks[&id].status, ChunkStatus::Pending);

        s.toggle_stage(id);
        assert_eq!(s.doc.chunks[&id].status, ChunkStatus::Staged);

        s.toggle_stage(id);
        assert_eq!(s.doc.chunks[&id].status, ChunkStatus::Unstaged);

        s.toggle_stage(id);
        assert_eq!(s.doc.chunks[&id].status, ChunkStatus::Staged);
    }

    #[test]
    fn toggle_from_skipped_goes_to_staged() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\n", "a\nB\n");
        let id = ids[0];
        s.set_status(id, ChunkStatus::Skipped);
        s.toggle_stage(id);
        assert_eq!(s.doc.chunks[&id].status, ChunkStatus::Staged);
    }

    #[test]
    fn progress_counts_buckets() {
        let mut s = in_memory_session();
        // Three changes separated by >7 lines each so context=3 can't merge
        // them into fewer hunks.
        let base: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let mut buffer_lines: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
        buffer_lines[0] = "LINE0".into();
        buffer_lines[10] = "LINE10".into();
        buffer_lines[20] = "LINE20".into();
        let buffer: String = buffer_lines
            .into_iter()
            .flat_map(|l| [l, "\n".to_string()])
            .collect();
        let ids = add(&mut s, "a.txt", &base, &buffer);
        assert_eq!(ids.len(), 3);
        s.set_status(ids[0], ChunkStatus::Staged);
        s.set_status(ids[1], ChunkStatus::Unstaged);
        // ids[2] remains Pending

        let p = s.progress();
        assert_eq!(
            p,
            ReviewProgress {
                staged: 1,
                unstaged: 1,
                skipped: 0,
                pending: 1,
                total: 3,
                current_index: Some(1),
            }
        );
    }

    #[test]
    fn is_complete_when_all_decided() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        assert!(!s.is_complete());
        s.set_status(ids[0], ChunkStatus::Staged);
        assert!(!s.is_complete());
        s.set_status(ids[1], ChunkStatus::Skipped);
        assert!(s.is_complete());
    }

    #[test]
    fn multi_file_navigation_spans_files() {
        let mut s = in_memory_session();
        let a = add(&mut s, "a.txt", "a\nb\n", "A\nb\n");
        let b = add(&mut s, "b.txt", "c\nd\n", "c\nD\n");
        assert_eq!(s.order, [a[0], b[0]]);
        assert_eq!(s.cursor.current, Some(a[0]));
        assert_eq!(s.next(), Some(b[0]));
        assert_eq!(s.current().map(|c| c.file_index), Some(1));
        assert_eq!(s.current().map(|c| c.chunk_index_in_file), Some(0));
    }

    #[test]
    fn version_bumps_on_mutation() {
        let mut s = in_memory_session();
        let v0 = s.version;
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        assert!(s.version > v0);

        let v1 = s.version;
        s.set_status(ids[0], ChunkStatus::Staged);
        assert!(s.version > v1);

        let v2 = s.version;
        s.next();
        assert!(s.version > v2);
    }

    #[test]
    fn line_and_byte_ranges_cover_changes() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let chunk = &s.doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 1..2);
        assert_eq!(chunk.buffer_line_range, 1..2);
        assert_eq!(chunk.base_byte_range, 2..5);
        assert_eq!(chunk.buffer_byte_range, 2..5);
    }

    #[test]
    fn pure_addition_has_empty_base_range() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\n", "a\nNEW\nb\n");
        let chunk = &s.doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 0..0);
        assert_eq!(chunk.buffer_line_range, 1..2);
    }

    #[test]
    fn pure_deletion_has_empty_buffer_range() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nb\n", "a\nb\n");
        let chunk = &s.doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 1..2);
        assert_eq!(chunk.buffer_line_range, 0..0);
    }

    #[test]
    fn identity_key_is_stable_across_equal_content() {
        let mut s1 = in_memory_session();
        let ids1 = add(&mut s1, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let k1 = s1.identity_key(ids1[0]).unwrap();

        let mut s2 = in_memory_session();
        let ids2 = add(&mut s2, "a.txt", "a\nOLD\nc\n", "a\nDIFF\nc\n");
        let k2 = s2.identity_key(ids2[0]).unwrap();

        assert_eq!(k1, k2);
    }

    #[test]
    fn view_state_covers_full_file_with_gap_rows() {
        let mut s = in_memory_session();
        let a = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(a.len(), 2);
        let view = ReviewViewState::from_session(&s);

        // Every one of the 11 file lines renders. The two 4-row hunks sit at
        // rows 0..4 and 7..11, with the 3 unchanged lines e, f, g filling the
        // gap between them.
        assert_eq!(view.rows.len(), 11);
        assert_eq!(view.chunk_row_ranges, vec![(a[0], 0..4), (a[1], 7..11)]);
        assert_eq!(view.session_version, s.version);
    }

    #[test]
    fn view_state_maps_rows_to_chunks() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        let view = ReviewViewState::from_session(&s);

        let chunk_of = |row| view.chunk_and_status_at_row(row).map(|(id, _)| id);

        assert_eq!(chunk_of(0), Some(ids[0]));
        assert_eq!(chunk_of(3), Some(ids[0]));
        assert_eq!(chunk_of(4), None, "the unchanged gap belongs to no chunk");
        assert_eq!(chunk_of(6), None, "the unchanged gap belongs to no chunk");
        assert_eq!(chunk_of(7), Some(ids[1]));
        assert_eq!(chunk_of(10), Some(ids[1]));

        assert_eq!(view.row_of_chunk(ids[0]), Some(0));
        assert_eq!(view.row_of_chunk(ids[1]), Some(7));
    }

    #[test]
    fn view_state_renders_added_file_all_added() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "new.txt", "", "x\ny\nz\n");
        let view = ReviewViewState::from_session(&s);

        // An empty base leaves no line to walk on the left, so every row is
        // all-added. Each has a right side and an absent left one, and a single
        // chunk covers them all.
        assert_eq!(view.rows.len(), 3);
        assert!(
            view.rows.iter().all(|r| matches!(
                r,
                ReviewRow::Changed {
                    left: None,
                    right: Some(_)
                }
            )),
            "every row of a new file is all-added with no left side",
        );
        assert_eq!(view.chunk_row_ranges, vec![(ids[0], 0..3)]);
    }

    #[test]
    fn identity_key_differs_when_base_changes() {
        let mut s1 = in_memory_session();
        let ids1 = add(&mut s1, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let k1 = s1.identity_key(ids1[0]).unwrap();

        let mut s2 = in_memory_session();
        let ids2 = add(&mut s2, "a.txt", "a\nDIFFERENT\nc\n", "a\nNEW\nc\n");
        let k2 = s2.identity_key(ids2[0]).unwrap();

        assert_ne!(k1, k2);
    }
}
