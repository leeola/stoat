use crate::{
    editor_state::EditorId,
    host::WatchToken,
    review::{
        extract_review_hunks_changeset, line_byte_offsets, split_lines, MoveProvenance,
        ReviewFileInput, ReviewHunk, ReviewRow,
    },
    review_apply,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use stoat_language::Language;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReviewChunkId(u32);

/// Content-derived identifier for a [`ReviewChunk`] that survives
/// across session boundaries. Workspace persistence keys the
/// `Staged` / `Unstaged` / `Skipped` decision map by fingerprint
/// so reopening the session re-applies decisions to chunks whose
/// content still matches, while chunks whose content drifted
/// (underlying file edited externally) come back as
/// [`ChunkStatus::Pending`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkFingerprint(pub [u8; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkStatus {
    Pending,
    Staged,
    Unstaged,
    Skipped,
}

impl ChunkStatus {
    pub fn is_decided(self) -> bool {
        matches!(
            self,
            ChunkStatus::Staged | ChunkStatus::Unstaged | ChunkStatus::Skipped
        )
    }
}

/// Provenance of the content under review.
///
/// - [`ReviewSource::WorkingTree`]: git index vs working tree of `workdir`.
/// - [`ReviewSource::WorkspaceWatch`]: live edits inside `workdir`, diffed per file against git
///   HEAD. The session starts empty and grows as filesystem-watch events arrive.
/// - [`ReviewSource::Commit`]: commit tree vs its parent (empty tree for a root commit).
/// - [`ReviewSource::CommitRange`]: `from..=to`, diff between the trees at the two commits,
///   inclusive of `to`.
/// - [`ReviewSource::AgentEdits`]: in-memory edit proposals; no repo required.
/// - [`ReviewSource::InMemory`]: test-only placeholder; not rescannable.
#[derive(Clone, Debug)]
pub enum ReviewSource {
    WorkingTree {
        workdir: PathBuf,
    },
    WorkspaceWatch {
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

/// Test-only / future-facing carrier for agent-proposed edits. Kept as a
/// concrete type rather than an opaque placeholder so the variant signature
/// does not churn when the real agent bridge lands.
#[derive(Clone, Debug)]
pub struct AgentEditProposal {
    pub path: PathBuf,
    pub base_text: Arc<String>,
    pub proposed_text: Arc<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct InMemoryFile {
    pub path: PathBuf,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
}

#[derive(Clone, Debug)]
pub struct ReviewChunk {
    #[allow(dead_code)]
    pub id: ReviewChunkId,
    pub file_index: usize,
    pub chunk_index_in_file: usize,
    pub hunk: ReviewHunk,
    /// 0-based half-open row range in the buffer (RHS) text. Empty for
    /// pure-deletion chunks; callers scrolling to a chunk should fall
    /// back to `base_line_range` in that case.
    #[allow(dead_code)]
    pub buffer_line_range: Range<u32>,
    /// 0-based half-open row range in the base (LHS) text.
    pub base_line_range: Range<u32>,
    #[allow(dead_code)]
    pub buffer_byte_range: Range<usize>,
    pub base_byte_range: Range<usize>,
    pub status: ChunkStatus,
    /// Reviewer's approval of the chunk, independent of [`Self::status`].
    /// Lets a reviewer mark a chunk inspected without committing to a
    /// staging decision -- the v2 review workflow advanced the cursor
    /// through approval state rather than through `Staged`/`Unstaged`.
    pub approved: bool,
}

impl ReviewChunk {
    /// Stable content-derived identifier for workspace
    /// persistence. Hashes each row's left + right text with
    /// presence markers + variant tags + a `\0` delimiter, then
    /// mixes in [`Self::base_line_range`]'s start and end as
    /// little-endian `u32`s. The buffer-side ranges are
    /// deliberately excluded -- they shift when neighbouring
    /// chunks change, while the base side stays pinned to the
    /// pre-image until the source actually changes.
    pub fn fingerprint(&self) -> ChunkFingerprint {
        let mut hasher = blake3::Hasher::new();
        for row in &self.hunk.rows {
            match row {
                ReviewRow::Context { left, right } => {
                    hasher.update(b"\x01");
                    hasher.update(left.text.as_bytes());
                    hasher.update(b"\x00");
                    hasher.update(right.text.as_bytes());
                },
                ReviewRow::Changed { left, right } => {
                    hasher.update(b"\x02");
                    match left {
                        Some(side) => {
                            hasher.update(b"L");
                            hasher.update(side.text.as_bytes());
                        },
                        None => {
                            hasher.update(b"l");
                        },
                    }
                    hasher.update(b"\x00");
                    match right {
                        Some(side) => {
                            hasher.update(b"R");
                            hasher.update(side.text.as_bytes());
                        },
                        None => {
                            hasher.update(b"r");
                        },
                    }
                },
            }
        }
        hasher.update(&self.base_line_range.start.to_le_bytes());
        hasher.update(&self.base_line_range.end.to_le_bytes());
        ChunkFingerprint(hasher.finalize().into())
    }
}

#[derive(Clone)]
pub struct ReviewFile {
    pub path: PathBuf,
    pub rel_path: String,
    pub language: Option<Arc<Language>>,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
    pub chunks: Vec<ReviewChunkId>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReviewCursor {
    pub current: Option<ReviewChunkId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReviewProgress {
    pub staged: usize,
    pub unstaged: usize,
    pub skipped: usize,
    pub pending: usize,
    pub total: usize,
    /// Count of chunks whose `approved` flag is `true`. Independent of
    /// `staged`/`unstaged`/`skipped`/`pending` -- a chunk can be both
    /// approved and any status, so this can exceed [`Self::staged`].
    pub approved: usize,
    /// 1-based index of `cursor.current` within the flattened order, or
    /// `None` if the cursor has not settled on a chunk yet.
    pub current_index: Option<usize>,
}

impl ReviewProgress {
    /// True when the session has at least one chunk and every chunk
    /// has been decided (staged, unstaged, or skipped).
    pub fn is_complete(&self) -> bool {
        self.total > 0 && self.pending == 0
    }
}

/// UI-facing cache derived from a [`ReviewSession`]. Attached to an editor
/// so `render_review` can paint without walking the session on every frame,
/// and so navigation handlers can map a chunk id to a display row.
#[derive(Clone, Debug)]
pub struct ReviewViewState {
    /// Flattened rows across every file's chunks, in visit order. One row
    /// per placeholder-buffer line.
    pub rows: Vec<ReviewRow>,
    /// (chunk_id, first_display_row) ordered by display row. Used both for
    /// row lookup and for chunk-to-scroll-row lookup.
    pub chunk_row_starts: Vec<(ReviewChunkId, u32)>,
    /// Status of each chunk, indexed parallel to `chunk_row_starts`. Kept
    /// here so `render_review` can paint gutter glyphs without holding a
    /// reference to the session.
    pub chunk_statuses: Vec<ChunkStatus>,
    /// Chunk currently under the review cursor, if any. Rendered with an
    /// additional highlight so the user can tell which chunk their
    /// navigation keys will act on.
    pub current_chunk: Option<ReviewChunkId>,
    /// Session version this cache was built from.
    pub session_version: u64,
}

impl ReviewViewState {
    pub fn from_session(session: &ReviewSession) -> Self {
        let mut rows: Vec<ReviewRow> = Vec::new();
        let mut chunk_row_starts: Vec<(ReviewChunkId, u32)> = Vec::new();
        let mut chunk_statuses: Vec<ChunkStatus> = Vec::new();
        for file in &session.files {
            for chunk_id in &file.chunks {
                if let Some(chunk) = session.chunks.get(chunk_id) {
                    chunk_row_starts.push((*chunk_id, rows.len() as u32));
                    chunk_statuses.push(chunk.status);
                    rows.extend(chunk.hunk.rows.iter().cloned());
                }
            }
        }
        Self {
            rows,
            chunk_row_starts,
            chunk_statuses,
            current_chunk: session.cursor.current,
            session_version: session.version,
        }
    }

    /// Sync the status cache and cursor from the session without rebuilding
    /// row data. Cheaper than `from_session` and the right call when only
    /// the cursor or a chunk's status has changed.
    pub fn refresh_from_session(&mut self, session: &ReviewSession) {
        if self.session_version == session.version {
            return;
        }
        self.chunk_statuses.clear();
        self.chunk_statuses.reserve(self.chunk_row_starts.len());
        for (id, _) in &self.chunk_row_starts {
            let status = session
                .chunks
                .get(id)
                .map(|c| c.status)
                .unwrap_or(ChunkStatus::Pending);
            self.chunk_statuses.push(status);
        }
        self.current_chunk = session.cursor.current;
        self.session_version = session.version;
    }

    /// Returns the (chunk_id, status) for the given display row, if any.
    pub fn chunk_and_status_at_row(&self, row: u32) -> Option<(ReviewChunkId, ChunkStatus)> {
        let idx = self
            .chunk_row_starts
            .partition_point(|(_, start)| *start <= row)
            .checked_sub(1)?;
        let (id, _) = self.chunk_row_starts[idx];
        let status = self.chunk_statuses.get(idx).copied()?;
        Some((id, status))
    }

    /// Returns the first display row of the given chunk, or `None` if the
    /// chunk is not represented in this view.
    pub fn row_of_chunk(&self, id: ReviewChunkId) -> Option<u32> {
        self.chunk_row_starts
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, r)| *r)
    }
}

/// What surface the review was opened from, used to decide where
/// `CloseReview` should land the user (normal mode vs. back to the
/// commit-list view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReviewOrigin {
    /// Opened directly from normal mode (e.g. `OpenReview` or
    /// `OpenReviewCommit` from the palette). `CloseReview` returns to
    /// normal mode.
    #[default]
    Standalone,
    /// Opened from a `CommitsOpenReview` dispatch while the user was in
    /// commits mode. `CloseReview` restores commits mode with the
    /// previously selected commit still highlighted.
    FromCommits,
    /// Opened by the rebase stepper during an `Edit` pause. The
    /// session's source sha may change as the user invokes
    /// `ReviewRemoveSelected` to refine the commit; `RebaseContinue`
    /// picks up whatever sha the session currently points at.
    FromRebaseEdit,
}

// FIXME: Per-chunk Staged/Unstaged/Skipped status not persisted across
// workspace save/load. [`ReviewChunkId`] is allocated fresh per session, so we
// cannot simply serialize the HashMap keyed on it. Resolution: assign each
// chunk a stable fingerprint (e.g. blake3 of pre+post content + base line
// range) at chunk-creation time, persist a
// `HashMap<ChunkFingerprint, ChunkStatus>`, and re-key on load. Chunks whose
// fingerprint no longer matches (underlying file changed externally) degrade
// to `Pending`.
#[derive(Clone)]
pub struct ReviewSession {
    pub source: ReviewSource,
    pub files: Vec<ReviewFile>,
    pub chunks: HashMap<ReviewChunkId, ReviewChunk>,
    pub order: Vec<ReviewChunkId>,
    pub cursor: ReviewCursor,
    pub view_editor: Option<EditorId>,
    /// Bumped on any mutation so editor-level caches can detect staleness.
    pub version: u64,
    /// Where the user launched this review from; consulted on close.
    pub origin: ReviewOrigin,
    /// Filesystem-watch tokens covering each path in [`Self::files`].
    /// Populated only when [`Self::source`] is
    /// [`ReviewSource::WorkingTree`]; other sources skip watching
    /// because their content is not on disk.
    pub watch_tokens: Vec<WatchToken>,
    next_id: u32,
}

impl ReviewSession {
    pub fn new(source: ReviewSource) -> Self {
        Self {
            source,
            files: Vec::new(),
            chunks: HashMap::new(),
            order: Vec::new(),
            cursor: ReviewCursor::default(),
            view_editor: None,
            version: 0,
            origin: ReviewOrigin::Standalone,
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
    pub fn add_files(&mut self, files: Vec<ReviewFileInput>) -> Vec<Vec<ReviewChunkId>> {
        let hunks_per_file = extract_review_hunks_changeset(&files, 3);
        let mut all_chunk_ids: Vec<Vec<ReviewChunkId>> = Vec::with_capacity(files.len());

        for (file, hunks) in files.into_iter().zip(hunks_per_file) {
            let file_index = self.files.len();

            let base_offsets = line_byte_offsets(&split_lines(&file.base_text));
            let buffer_offsets = line_byte_offsets(&split_lines(&file.buffer_text));

            let mut chunk_ids: Vec<ReviewChunkId> = Vec::with_capacity(hunks.len());
            for (chunk_index_in_file, hunk) in hunks.into_iter().enumerate() {
                let id = self.alloc_id();
                let (base_line_range, buffer_line_range) = hunk_line_ranges(&hunk);
                let base_byte_range = lines_to_bytes(&base_offsets, &base_line_range);
                let buffer_byte_range = lines_to_bytes(&buffer_offsets, &buffer_line_range);

                self.chunks.insert(
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
                        approved: false,
                    },
                );
                self.order.push(id);
                chunk_ids.push(id);
            }

            self.files.push(ReviewFile {
                path: file.path,
                rel_path: file.rel_path,
                language: file.language,
                base_text: file.base_text,
                buffer_text: file.buffer_text,
                chunks: chunk_ids.clone(),
            });

            all_chunk_ids.push(chunk_ids);
        }

        if self.cursor.current.is_none() {
            self.cursor.current = self.order.first().copied();
        }

        self.version += 1;
        all_chunk_ids
    }

    #[allow(dead_code)]
    pub fn chunk(&self, id: ReviewChunkId) -> Option<&ReviewChunk> {
        self.chunks.get(&id)
    }

    /// Snapshot every non-Pending chunk's status keyed by stable
    /// [`ChunkFingerprint`] for workspace persistence. Pending
    /// chunks are dropped from the snapshot so the on-disk map
    /// only records explicit user decisions; loading is then a
    /// best-effort re-application via [`Self::apply_statuses`].
    pub fn snapshot_statuses(&self) -> HashMap<ChunkFingerprint, ChunkStatus> {
        self.chunks
            .values()
            .filter(|c| c.status != ChunkStatus::Pending)
            .map(|c| (c.fingerprint(), c.status))
            .collect()
    }

    /// Apply persisted statuses keyed by [`ChunkFingerprint`].
    /// Chunks whose fingerprint is in `statuses` adopt the saved
    /// status; chunks whose fingerprint is absent (underlying
    /// file edited externally since the snapshot was taken) stay
    /// at [`ChunkStatus::Pending`]. Bumps [`Self::version`] when
    /// at least one chunk was updated so derived caches refresh.
    /// Returns the count of chunks whose status was restored.
    pub fn apply_statuses(&mut self, statuses: &HashMap<ChunkFingerprint, ChunkStatus>) -> usize {
        let mut applied = 0usize;
        for chunk in self.chunks.values_mut() {
            let fp = chunk.fingerprint();
            if let Some(&status) = statuses.get(&fp) {
                if chunk.status != status {
                    chunk.status = status;
                    applied += 1;
                }
            }
        }
        if applied > 0 {
            self.version += 1;
        }
        applied
    }

    /// Snapshot approval flags keyed by [`ChunkFingerprint`] for
    /// workspace persistence. Only chunks with `approved == true`
    /// appear in the map so the on-disk record stays sparse and
    /// reloading is a best-effort re-application via
    /// [`Self::apply_approvals`].
    pub fn snapshot_approvals(&self) -> HashMap<ChunkFingerprint, bool> {
        self.chunks
            .values()
            .filter(|c| c.approved)
            .map(|c| (c.fingerprint(), true))
            .collect()
    }

    /// Apply persisted approval flags keyed by [`ChunkFingerprint`].
    /// Chunks whose fingerprint is in `approvals` adopt the saved
    /// flag; chunks whose fingerprint is absent stay at their current
    /// value (no automatic reset to `false`, matching how
    /// [`Self::apply_statuses`] leaves unmatched chunks alone).
    /// Bumps [`Self::version`] when any chunk changes so derived
    /// caches refresh. Returns the count of chunks whose flag was
    /// restored.
    pub fn apply_approvals(&mut self, approvals: &HashMap<ChunkFingerprint, bool>) -> usize {
        let mut applied = 0usize;
        for chunk in self.chunks.values_mut() {
            let fp = chunk.fingerprint();
            if let Some(&approved) = approvals.get(&fp) {
                if chunk.approved != approved {
                    chunk.approved = approved;
                    applied += 1;
                }
            }
        }
        if applied > 0 {
            self.version += 1;
        }
        applied
    }

    /// Resolve an in-buffer byte offset to the chunk that should
    /// receive cursor focus. A chunk whose `buffer_byte_range` covers
    /// the byte wins outright; otherwise the first chunk starting at
    /// or after the byte; otherwise the file's last chunk so callers
    /// past every existing hunk still get a navigation target.
    /// Returns `None` only when `file_index` is out of range or the
    /// file has no chunks.
    pub fn chunk_containing_buffer_byte(
        &self,
        file_index: usize,
        buffer_byte: usize,
    ) -> Option<ReviewChunkId> {
        let file = self.files.get(file_index)?;
        let mut last: Option<ReviewChunkId> = None;
        for id in &file.chunks {
            let chunk = self.chunks.get(id)?;
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
    pub fn current(&self) -> Option<&ReviewChunk> {
        self.cursor.current.and_then(|id| self.chunks.get(&id))
    }

    /// Walk the chunk's rows and collect every distinct
    /// [`MoveProvenance`] attached to the RHS side -- the
    /// counterpart source location for content moved INTO this
    /// chunk. Order preserved; duplicates collapsed by
    /// `(rel_path, line)` so an N-row chunk pointing at one
    /// source returns one entry.
    pub fn move_sources_in_chunk(&self, id: ReviewChunkId) -> Vec<MoveProvenance> {
        let Some(chunk) = self.chunks.get(&id) else {
            return Vec::new();
        };
        let mut out: Vec<MoveProvenance> = Vec::new();
        for row in &chunk.hunk.rows {
            let ReviewRow::Changed { right, .. } = row else {
                continue;
            };
            let Some(right) = right else { continue };
            let Some(prov) = right.move_provenance.as_ref() else {
                continue;
            };
            if !out.iter().any(|p| p == prov) {
                out.push(prov.clone());
            }
        }
        out
    }

    /// Walk the chunk's rows and collect every distinct
    /// [`MoveProvenance`] attached to the LHS side of an LHS-only
    /// row -- the target location for content moved OUT of this
    /// chunk. Order preserved; duplicates collapsed by
    /// `(rel_path, line)`.
    pub fn move_targets_in_chunk(&self, id: ReviewChunkId) -> Vec<MoveProvenance> {
        let Some(chunk) = self.chunks.get(&id) else {
            return Vec::new();
        };
        let mut out: Vec<MoveProvenance> = Vec::new();
        for row in &chunk.hunk.rows {
            let ReviewRow::Changed { left, right } = row else {
                continue;
            };
            if right.is_some() {
                continue;
            }
            let Some(left) = left else { continue };
            let Some(prov) = left.move_provenance.as_ref() else {
                continue;
            };
            if !out.iter().any(|p| p == prov) {
                out.push(prov.clone());
            }
        }
        out
    }

    /// Walk every chunk in the session and collect every distinct
    /// cross-file move as a [`MoveRelationship`] pair. Both
    /// `right.move_provenance` on RHS-bearing rows and
    /// `left.move_provenance` on LHS-only rows feed the same
    /// dedup set keyed by `(source_path, source_line,
    /// target_path, target_line)`, so a multi-row move shows
    /// once and the two sources of truth converge. Order
    /// preserved (first occurrence wins).
    pub fn collect_move_relationships(&self) -> Vec<MoveRelationship> {
        let mut out: Vec<MoveRelationship> = Vec::new();
        for id in &self.order {
            let Some(chunk) = self.chunks.get(id) else {
                continue;
            };
            let Some(file) = self.files.get(chunk.file_index) else {
                continue;
            };
            for row in &chunk.hunk.rows {
                let ReviewRow::Changed { left, right } = row else {
                    continue;
                };
                if let Some(right) = right {
                    if let Some(prov) = right.move_provenance.as_ref() {
                        let rel = MoveRelationship {
                            source: prov.clone(),
                            target: MoveProvenance {
                                rel_path: file.rel_path.clone(),
                                line: right.line_num.saturating_sub(1),
                            },
                        };
                        if !out.contains(&rel) {
                            out.push(rel);
                        }
                    }
                } else if let Some(left) = left {
                    if let Some(prov) = left.move_provenance.as_ref() {
                        let rel = MoveRelationship {
                            source: MoveProvenance {
                                rel_path: file.rel_path.clone(),
                                line: left.line_num.saturating_sub(1),
                            },
                            target: prov.clone(),
                        };
                        if !out.contains(&rel) {
                            out.push(rel);
                        }
                    }
                }
            }
        }
        out
    }

    /// Resolve a `(file_index, buffer_line)` pair to a chunk in
    /// that file. Returns the chunk whose `buffer_line_range`
    /// contains `line`; otherwise the first chunk that starts at
    /// or after `line`; otherwise the file's last chunk so
    /// out-of-range navigation still produces a stable cursor
    /// target. Returns `None` only when `file_index` is out of
    /// range or the file has no chunks.
    pub fn chunk_for_buffer_line(&self, file_index: usize, line: u32) -> Option<ReviewChunkId> {
        let file = self.files.get(file_index)?;
        let mut last: Option<ReviewChunkId> = None;
        for id in &file.chunks {
            let chunk = self.chunks.get(id)?;
            if chunk.buffer_line_range.contains(&line) {
                return Some(*id);
            }
            if chunk.buffer_line_range.start >= line {
                return Some(*id);
            }
            last = Some(*id);
        }
        last
    }

    /// Advance the cursor to the next chunk. Clamps at the last chunk and
    /// returns `None` when already there (callers may surface this as an
    /// "end of review" signal).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<ReviewChunkId> {
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

    pub fn prev(&mut self) -> Option<ReviewChunkId> {
        let idx = self.cursor_order_index()?;
        if idx == 0 {
            return None;
        }
        let id = self.order[idx - 1];
        self.cursor.current = Some(id);
        self.version += 1;
        Some(id)
    }

    /// Move the cursor to the next chunk whose `approved` flag is
    /// `false`, wrapping from the end of `order` back to `0` if no
    /// unapproved chunk lies past the current position. Returns
    /// `Some(id)` when the cursor moves, `None` when every chunk
    /// is approved or the session is empty. The cursor stays put
    /// when only the current chunk is unapproved.
    pub fn next_unreviewed(&mut self) -> Option<ReviewChunkId> {
        if self.order.is_empty() {
            return None;
        }
        let start = self.cursor_order_index().map(|i| i + 1).unwrap_or(0);
        let len = self.order.len();
        for offset in 0..len {
            let i = (start + offset) % len;
            let id = self.order[i];
            let unapproved = self.chunks.get(&id).is_some_and(|c| !c.approved);
            if unapproved && Some(id) != self.cursor.current {
                self.cursor.current = Some(id);
                self.version += 1;
                return Some(id);
            }
        }
        None
    }

    pub fn set_status(&mut self, id: ReviewChunkId, status: ChunkStatus) {
        if let Some(chunk) = self.chunks.get_mut(&id) {
            chunk.status = status;
            self.version += 1;
        }
    }

    /// Set the chunk's approval flag and bump [`Self::version`] only
    /// when the value actually changes. Independent of
    /// [`Self::set_status`] -- approving a chunk does not stage it.
    pub fn set_approved(&mut self, id: ReviewChunkId, approved: bool) {
        if let Some(chunk) = self.chunks.get_mut(&id) {
            if chunk.approved != approved {
                chunk.approved = approved;
                self.version += 1;
            }
        }
    }

    /// Flip the chunk's approval flag and bump [`Self::version`].
    /// Toggling a missing chunk is silently a no-op.
    pub fn toggle_approved(&mut self, id: ReviewChunkId) {
        if let Some(chunk) = self.chunks.get_mut(&id) {
            chunk.approved = !chunk.approved;
            self.version += 1;
        }
    }

    /// Move the review cursor back to the first chunk in
    /// [`Self::order`]. Bumps [`Self::version`] so derived caches
    /// refresh. Becomes `None` only when the session has no chunks.
    pub fn reset_cursor(&mut self) {
        self.cursor.current = self.order.first().copied();
        self.version += 1;
    }

    /// Clear approval and revert status to `Pending` for every chunk,
    /// then snap the cursor back to the first chunk. The reviewer
    /// uses this to start a session over. Always bumps
    /// [`Self::version`] so observers refresh even when the session
    /// was already clean.
    pub fn reset_progress(&mut self) {
        for chunk in self.chunks.values_mut() {
            chunk.status = ChunkStatus::Pending;
            chunk.approved = false;
        }
        self.cursor.current = self.order.first().copied();
        self.version += 1;
    }

    /// Add, replace, or drop the entry for `input.path` based on
    /// the file's freshly-computed single-file diff. The watch-mode
    /// event loop calls this on each `FsWatchEvent` for an in-scope
    /// path. Returns the new chunk ids for the file (empty when the
    /// file was dropped or when an empty-diff upsert hit a path the
    /// session did not already contain).
    ///
    /// - When `input.path` is not yet in [`Self::files`] and the new diff is non-empty, appends a
    ///   new file entry and its chunks.
    /// - When `input.path` is already in [`Self::files`], drops the file's prior chunks and
    ///   re-extracts in place. Decided statuses carry across when [`Self::identity_key`] matches;
    ///   if the cursor was on this file, it sticks to a new chunk with the same identity, else the
    ///   first chunk in the refreshed file.
    /// - When the new diff is empty and the file is in the session, drops the file entry entirely.
    ///   Later files' `file_index` shifts down by one to keep `chunks` consistent with `files`. If
    ///   the cursor was on this file, falls back to the first remaining chunk in `order` (or `None`
    ///   when empty).
    /// - When the new diff is empty and the file is not in the session, the call is a no-op (no
    ///   version bump).
    ///
    /// Other files in the session are untouched: their chunk ids,
    /// statuses, and texts survive. Cross-file move metadata is
    /// computed only against the single upserted file, so prior
    /// cross-file move chips referencing other paths may go stale
    /// until a whole-session refresh.
    pub fn upsert_file(&mut self, input: ReviewFileInput) -> Vec<ReviewChunkId> {
        let existing_index = self.files.iter().position(|f| f.path == input.path);

        let inputs = vec![input];
        let mut hunks_per_file = extract_review_hunks_changeset(&inputs, 3);
        let input = inputs.into_iter().next().expect("single input");
        let hunks = hunks_per_file.pop().unwrap_or_default();

        if hunks.is_empty() {
            let Some(idx) = existing_index else {
                return Vec::new();
            };
            let old_chunk_ids: Vec<ReviewChunkId> = self.files[idx].chunks.clone();
            let cursor_was_in_file = self
                .cursor
                .current
                .map(|id| old_chunk_ids.contains(&id))
                .unwrap_or(false);
            for id in &old_chunk_ids {
                self.chunks.remove(id);
            }
            self.order.retain(|id| !old_chunk_ids.contains(id));
            self.files.remove(idx);
            for chunk in self.chunks.values_mut() {
                if chunk.file_index > idx {
                    chunk.file_index -= 1;
                }
            }
            if cursor_was_in_file {
                self.cursor.current = self.order.first().copied();
            }
            self.version += 1;
            return Vec::new();
        }

        let session_had_no_cursor = self.cursor.current.is_none();
        let carried: HashMap<ChunkIdentity, ChunkStatus>;
        let prev_cursor_ident: Option<ChunkIdentity>;
        let cursor_was_in_file: bool;
        let order_insert_pos: usize;
        let file_index: usize;

        if let Some(idx) = existing_index {
            let old_chunk_ids: Vec<ReviewChunkId> = self.files[idx].chunks.clone();
            carried = old_chunk_ids
                .iter()
                .filter_map(|id| {
                    let status = self.chunks.get(id)?.status;
                    if !status.is_decided() {
                        return None;
                    }
                    let ident = self.identity_key(*id)?;
                    Some((ident, status))
                })
                .collect();
            prev_cursor_ident = self
                .cursor
                .current
                .filter(|id| old_chunk_ids.contains(id))
                .and_then(|id| self.identity_key(id));
            cursor_was_in_file = self
                .cursor
                .current
                .map(|id| old_chunk_ids.contains(&id))
                .unwrap_or(false);
            order_insert_pos = old_chunk_ids
                .first()
                .and_then(|first| self.order.iter().position(|id| id == first))
                .unwrap_or(self.order.len());
            for id in &old_chunk_ids {
                self.chunks.remove(id);
            }
            self.order.retain(|id| !old_chunk_ids.contains(id));
            file_index = idx;
        } else {
            carried = HashMap::new();
            prev_cursor_ident = None;
            cursor_was_in_file = false;
            order_insert_pos = self.order.len();
            file_index = self.files.len();
        }

        let base_offsets = line_byte_offsets(&split_lines(&input.base_text));
        let buffer_offsets = line_byte_offsets(&split_lines(&input.buffer_text));

        let mut new_chunk_ids: Vec<ReviewChunkId> = Vec::with_capacity(hunks.len());
        for (chunk_index_in_file, hunk) in hunks.into_iter().enumerate() {
            let id = self.alloc_id();
            let (base_line_range, buffer_line_range) = hunk_line_ranges(&hunk);
            let base_byte_range = lines_to_bytes(&base_offsets, &base_line_range);
            let buffer_byte_range = lines_to_bytes(&buffer_offsets, &buffer_line_range);
            self.chunks.insert(
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
                    approved: false,
                },
            );
            new_chunk_ids.push(id);
        }

        for (i, id) in new_chunk_ids.iter().enumerate() {
            self.order.insert(order_insert_pos + i, *id);
        }

        let new_file = ReviewFile {
            path: input.path,
            rel_path: input.rel_path,
            language: input.language,
            base_text: input.base_text,
            buffer_text: input.buffer_text,
            chunks: new_chunk_ids.clone(),
        };
        if let Some(idx) = existing_index {
            self.files[idx] = new_file;
        } else {
            self.files.push(new_file);
        }

        for id in &new_chunk_ids {
            let Some(ident) = self.identity_key(*id) else {
                continue;
            };
            if let Some(status) = carried.get(&ident).copied() {
                self.set_status(*id, status);
            }
        }

        if cursor_was_in_file || session_had_no_cursor {
            let by_identity = prev_cursor_ident.and_then(|ident| {
                new_chunk_ids
                    .iter()
                    .find(|id| self.identity_key(**id).as_ref() == Some(&ident))
                    .copied()
            });
            self.cursor.current = by_identity.or_else(|| new_chunk_ids.first().copied());
        }

        self.version += 1;
        new_chunk_ids
    }

    /// Replace the entry for `path` with `new_input` and
    /// re-extract its hunks in isolation. Other files in the
    /// session are untouched; cross-file move metadata referencing
    /// the refreshed file may go stale until a whole-session
    /// refresh fires.
    ///
    /// No-op when `path` is not currently in [`Self::files`].
    /// Decided statuses on the file's prior chunks carry across
    /// when the new chunks' [`Self::identity_key`] matches.
    /// Cursor behavior: if the cursor was on a chunk in this file,
    /// it sticks to a new chunk with the same identity (else the
    /// first chunk in the refreshed file); if the session had no
    /// cursor, it adopts the first new chunk; cursors on other
    /// files are left alone.
    pub fn refresh_file(&mut self, path: &std::path::Path, new_input: ReviewFileInput) {
        let Some(file_index) = self.files.iter().position(|f| f.path == path) else {
            return;
        };

        let old_chunk_ids: Vec<ReviewChunkId> = self.files[file_index].chunks.clone();
        let carried: HashMap<ChunkIdentity, ChunkStatus> = old_chunk_ids
            .iter()
            .filter_map(|id| {
                let status = self.chunks.get(id)?.status;
                if !status.is_decided() {
                    return None;
                }
                let ident = self.identity_key(*id)?;
                Some((ident, status))
            })
            .collect();
        let prev_cursor_ident = self
            .cursor
            .current
            .filter(|id| old_chunk_ids.contains(id))
            .and_then(|id| self.identity_key(id));
        let cursor_was_in_file = self
            .cursor
            .current
            .map(|id| old_chunk_ids.contains(&id))
            .unwrap_or(false);
        let session_had_no_cursor = self.cursor.current.is_none();

        let order_insert_pos = old_chunk_ids
            .first()
            .and_then(|first| self.order.iter().position(|id| id == first))
            .unwrap_or(self.order.len());
        for id in &old_chunk_ids {
            self.chunks.remove(id);
        }
        self.order.retain(|id| !old_chunk_ids.contains(id));

        let inputs = vec![new_input];
        let mut hunks_per_file = extract_review_hunks_changeset(&inputs, 3);
        let new_input = inputs.into_iter().next().expect("single input");
        let hunks = hunks_per_file.pop().unwrap_or_default();

        let base_offsets = line_byte_offsets(&split_lines(&new_input.base_text));
        let buffer_offsets = line_byte_offsets(&split_lines(&new_input.buffer_text));

        let mut new_chunk_ids: Vec<ReviewChunkId> = Vec::with_capacity(hunks.len());
        for (chunk_index_in_file, hunk) in hunks.into_iter().enumerate() {
            let id = self.alloc_id();
            let (base_line_range, buffer_line_range) = hunk_line_ranges(&hunk);
            let base_byte_range = lines_to_bytes(&base_offsets, &base_line_range);
            let buffer_byte_range = lines_to_bytes(&buffer_offsets, &buffer_line_range);
            self.chunks.insert(
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
                    approved: false,
                },
            );
            new_chunk_ids.push(id);
        }

        for (i, id) in new_chunk_ids.iter().enumerate() {
            self.order.insert(order_insert_pos + i, *id);
        }

        self.files[file_index] = ReviewFile {
            path: new_input.path,
            rel_path: new_input.rel_path,
            language: new_input.language,
            base_text: new_input.base_text,
            buffer_text: new_input.buffer_text,
            chunks: new_chunk_ids.clone(),
        };

        for id in &new_chunk_ids {
            let Some(ident) = self.identity_key(*id) else {
                continue;
            };
            if let Some(status) = carried.get(&ident).copied() {
                self.set_status(*id, status);
            }
        }

        if cursor_was_in_file || session_had_no_cursor {
            let by_identity = prev_cursor_ident.and_then(|ident| {
                new_chunk_ids
                    .iter()
                    .find(|id| self.identity_key(**id).as_ref() == Some(&ident))
                    .copied()
            });
            self.cursor.current = by_identity.or_else(|| new_chunk_ids.first().copied());
        }

        self.version += 1;
    }

    /// Replace the session's files with `new_files`, re-extracting
    /// hunks via [`extract_review_hunks_changeset`]. Decided
    /// statuses carry across the refresh keyed by
    /// [`Self::identity_key`]; chunks whose identity changed (the
    /// surrounding base content moved) lose their carried status
    /// and revert to [`ChunkStatus::Pending`]. The cursor sticks to
    /// a chunk with the same identity as the prior cursor when
    /// such a chunk exists in the refreshed session; otherwise it
    /// settles on the first remaining [`ChunkStatus::Pending`]
    /// chunk, or `None` when no pending chunk exists.
    pub fn refresh_files(&mut self, new_files: Vec<ReviewFileInput>) {
        let prev_cursor_ident = self.cursor.current.and_then(|id| self.identity_key(id));
        let carried: HashMap<ChunkIdentity, ChunkStatus> = self
            .order
            .iter()
            .filter_map(|id| {
                let status = self.chunks.get(id)?.status;
                if !status.is_decided() {
                    return None;
                }
                let ident = self.identity_key(*id)?;
                Some((ident, status))
            })
            .collect();

        self.files.clear();
        self.chunks.clear();
        self.order.clear();
        self.cursor.current = None;

        self.add_files(new_files);

        let new_ids: Vec<_> = self.order.clone();
        for id in &new_ids {
            let Some(ident) = self.identity_key(*id) else {
                continue;
            };
            if let Some(status) = carried.get(&ident).copied() {
                self.set_status(*id, status);
            }
        }

        let cursor_by_identity = prev_cursor_ident.and_then(|ident| {
            new_ids
                .iter()
                .find(|id| self.identity_key(**id).as_ref() == Some(&ident))
                .copied()
        });
        let cursor_fallback = || {
            new_ids
                .iter()
                .find(|id| {
                    self.chunks
                        .get(id)
                        .map(|c| c.status == ChunkStatus::Pending)
                        .unwrap_or(false)
                })
                .copied()
        };
        self.cursor.current = cursor_by_identity.or_else(cursor_fallback);
        self.version += 1;
    }

    /// Toggle between `Staged` and `Unstaged` for the given chunk. Chunks
    /// currently in `Pending` or `Skipped` flip to `Staged`, giving users
    /// a one-key path from "not looked at" into the accept lane.
    pub fn toggle_stage(&mut self, id: ReviewChunkId) {
        if let Some(chunk) = self.chunks.get_mut(&id) {
            chunk.status = match chunk.status {
                ChunkStatus::Staged => ChunkStatus::Unstaged,
                ChunkStatus::Unstaged | ChunkStatus::Pending | ChunkStatus::Skipped => {
                    ChunkStatus::Staged
                },
            };
            self.version += 1;
        }
    }

    pub fn progress(&self) -> ReviewProgress {
        let mut p = ReviewProgress {
            total: self.order.len(),
            current_index: self.cursor_order_index().map(|i| i + 1),
            ..Default::default()
        };
        for id in &self.order {
            if let Some(chunk) = self.chunks.get(id) {
                match chunk.status {
                    ChunkStatus::Staged => p.staged += 1,
                    ChunkStatus::Unstaged => p.unstaged += 1,
                    ChunkStatus::Skipped => p.skipped += 1,
                    ChunkStatus::Pending => p.pending += 1,
                }
                if chunk.approved {
                    p.approved += 1;
                }
            }
        }
        p
    }

    pub fn is_complete(&self) -> bool {
        !self.order.is_empty()
            && self
                .order
                .iter()
                .filter_map(|id| self.chunks.get(id))
                .all(|c| c.status.is_decided())
    }

    /// Lookup key for carrying status across a refresh. Combines file path,
    /// base line range, and a content hash of the base text for the chunk
    /// so that a chunk surviving a refresh keeps its decision, while a
    /// chunk whose underlying content moved or changed is treated as new.
    pub fn identity_key(&self, id: ReviewChunkId) -> Option<ChunkIdentity> {
        let chunk = self.chunks.get(&id)?;
        let file = self.files.get(chunk.file_index)?;
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
pub struct ChunkIdentity {
    pub path: PathBuf,
    pub base_line_start: u32,
    pub base_line_end: u32,
    pub content_hash: u64,
}

/// One cross-file move surfaced by
/// [`ReviewSession::collect_move_relationships`]: `source` is the
/// origin location (where the content used to be) and `target`
/// is the destination location (where it now lives). Both sides
/// are paths within the same review session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRelationship {
    pub source: MoveProvenance,
    pub target: MoveProvenance,
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

/// Builds a single unified-diff patch covering `chunks`, suitable for
/// `git apply --cached`. With `reverse` set, the emitted patch is the
/// inverse of the forward one, so feeding it back through the same
/// apply path unstages what the forward patch staged.
///
/// Returns `None` when `session`'s source carries no working directory
/// (the in-memory and agent-edit sources), when `chunks` is empty, or
/// when none of the ids resolve to a chunk with a backing file. Ids
/// that do not resolve are skipped rather than aborting the patch.
pub fn build_chunk_patch(
    session: &ReviewSession,
    chunks: impl IntoIterator<Item = ReviewChunkId>,
    reverse: bool,
) -> Option<String> {
    let workdir = match &session.source {
        ReviewSource::WorkingTree { workdir }
        | ReviewSource::WorkspaceWatch { workdir }
        | ReviewSource::Commit { workdir, .. }
        | ReviewSource::CommitRange { workdir, .. } => workdir.as_path(),
        ReviewSource::AgentEdits { .. } | ReviewSource::InMemory { .. } => return None,
    };

    let mut patch = String::new();
    for id in chunks {
        let Some(chunk) = session.chunks.get(&id) else {
            continue;
        };
        let Some(file) = session.files.get(chunk.file_index) else {
            continue;
        };
        patch.push_str(&review_apply::chunk_to_unified_diff(
            file, chunk, workdir, reverse,
        ));
    }

    (!patch.is_empty()).then_some(patch)
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

    fn working_tree(workdir: &str) -> ReviewSession {
        ReviewSession::new(ReviewSource::WorkingTree {
            workdir: PathBuf::from(workdir),
        })
    }

    #[test]
    fn build_chunk_patch_reverse_inverts_forward() {
        let mut s = working_tree("/work");
        let id = add(&mut s, "a.txt", "a\nb\nc\n", "a\nX\nY\nc\n")[0];

        let fwd = build_chunk_patch(&s, [id], false).expect("forward patch");
        let rev = build_chunk_patch(&s, [id], true).expect("reverse patch");

        let body = |patch: &str, prefix: char| -> Vec<String> {
            patch
                .lines()
                .filter(|l| l.starts_with(prefix) && !l.starts_with("---") && !l.starts_with("+++"))
                .map(|l| l[1..].to_string())
                .collect()
        };
        // The reverse patch is the exact inverse of the forward patch -- its
        // removals are the forward's additions and vice versa -- so the two
        // applied in sequence leave the index unchanged.
        assert_eq!(body(&fwd, '+'), body(&rev, '-'));
        assert_eq!(body(&fwd, '-'), body(&rev, '+'));

        let ranges = |patch: &str| -> (String, String) {
            let h = patch
                .lines()
                .find(|l| l.starts_with("@@"))
                .expect("hunk header");
            let mid = h.trim_start_matches("@@ ").trim_end_matches(" @@");
            let mut p = mid.split(' ');
            (p.next().unwrap().to_string(), p.next().unwrap().to_string())
        };
        let (fwd_from, fwd_to) = ranges(&fwd);
        let (rev_from, rev_to) = ranges(&rev);
        assert_eq!(
            fwd_from.trim_start_matches('-'),
            rev_to.trim_start_matches('+')
        );
        assert_eq!(
            fwd_to.trim_start_matches('+'),
            rev_from.trim_start_matches('-')
        );
    }

    #[test]
    fn build_chunk_patch_without_workdir_source_is_none() {
        let mut s = in_memory_session();
        let id = add(&mut s, "a.txt", "a\nb\n", "a\nB\n")[0];
        assert_eq!(build_chunk_patch(&s, [id], false), None);
    }

    #[test]
    fn build_chunk_patch_concatenates_chunks_and_skips_empty() {
        let mut s = working_tree("/work");
        let a = add(&mut s, "a.txt", "a\nb\n", "a\nB\n")[0];
        let b = add(&mut s, "b.txt", "x\ny\n", "x\nY\n")[0];

        let combined = build_chunk_patch(&s, [a, b], false).expect("combined patch");
        assert!(combined.contains("diff --git a/a.txt b/a.txt"));
        assert!(combined.contains("diff --git a/b.txt b/b.txt"));

        assert_eq!(
            build_chunk_patch(&s, Vec::<ReviewChunkId>::new(), false),
            None
        );
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
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].chunks, ids);
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
        assert_eq!(s.chunks[&id].status, ChunkStatus::Pending);

        s.toggle_stage(id);
        assert_eq!(s.chunks[&id].status, ChunkStatus::Staged);

        s.toggle_stage(id);
        assert_eq!(s.chunks[&id].status, ChunkStatus::Unstaged);

        s.toggle_stage(id);
        assert_eq!(s.chunks[&id].status, ChunkStatus::Staged);
    }

    #[test]
    fn toggle_from_skipped_goes_to_staged() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\n", "a\nB\n");
        let id = ids[0];
        s.set_status(id, ChunkStatus::Skipped);
        s.toggle_stage(id);
        assert_eq!(s.chunks[&id].status, ChunkStatus::Staged);
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
                approved: 0,
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
        let chunk = &s.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 1..2);
        assert_eq!(chunk.buffer_line_range, 1..2);
        assert_eq!(chunk.base_byte_range, 2..5);
        assert_eq!(chunk.buffer_byte_range, 2..5);
    }

    #[test]
    fn pure_addition_has_empty_base_range() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nb\n", "a\nNEW\nb\n");
        let chunk = &s.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 0..0);
        assert_eq!(chunk.buffer_line_range, 1..2);
    }

    #[test]
    fn pure_deletion_has_empty_buffer_range() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nb\n", "a\nb\n");
        let chunk = &s.chunks[&ids[0]];
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
    fn view_state_flattens_rows_in_order() {
        let mut s = in_memory_session();
        let a = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(a.len(), 2);
        let view = ReviewViewState::from_session(&s);
        assert_eq!(view.chunk_row_starts.len(), 2);
        assert_eq!(view.chunk_row_starts[0].0, a[0]);
        assert_eq!(view.chunk_row_starts[0].1, 0);
        assert_eq!(view.chunk_row_starts[1].0, a[1]);
        assert_eq!(
            view.chunk_row_starts[1].1,
            s.chunks[&a[0]].hunk.rows.len() as u32
        );
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
        let first_chunk_len = s.chunks[&ids[0]].hunk.rows.len() as u32;

        assert_eq!(
            view.chunk_and_status_at_row(0).map(|(id, _)| id),
            Some(ids[0])
        );
        assert_eq!(
            view.chunk_and_status_at_row(first_chunk_len - 1)
                .map(|(id, _)| id),
            Some(ids[0]),
        );
        assert_eq!(
            view.chunk_and_status_at_row(first_chunk_len)
                .map(|(id, _)| id),
            Some(ids[1]),
        );

        assert_eq!(view.row_of_chunk(ids[0]), Some(0));
        assert_eq!(view.row_of_chunk(ids[1]), Some(first_chunk_len));
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

    fn refresh_input(path: &str, base: &str, buffer: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from(path),
            rel_path: path.to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }
    }

    #[test]
    fn refresh_files_preserves_decided_status_by_identity() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        s.set_status(ids[0], ChunkStatus::Staged);
        let ident = s.identity_key(ids[0]).unwrap();

        s.refresh_files(vec![refresh_input("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n")]);

        let new_id = s.order[0];
        assert_eq!(s.identity_key(new_id), Some(ident));
        assert_eq!(s.chunks[&new_id].status, ChunkStatus::Staged);
    }

    #[test]
    fn refresh_files_drops_pending_status_on_identity_mismatch() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        s.set_status(ids[0], ChunkStatus::Staged);

        s.refresh_files(vec![refresh_input(
            "a.txt",
            "a\nDIFFERENT\nc\n",
            "a\nNEW\nc\n",
        )]);

        let new_id = s.order[0];
        assert_eq!(
            s.chunks[&new_id].status,
            ChunkStatus::Pending,
            "identity mismatch must drop the carried Staged status",
        );
    }

    #[test]
    fn refresh_files_keeps_cursor_on_same_identity() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        s.next();
        let cursor_ident = s.identity_key(ids[1]).unwrap();

        s.refresh_files(vec![refresh_input(
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        )]);

        let new_cursor = s.cursor.current.expect("cursor present");
        assert_eq!(s.identity_key(new_cursor), Some(cursor_ident));
    }

    #[test]
    fn refresh_files_resets_cursor_to_first_pending_when_identity_lost() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        s.set_status(ids[0], ChunkStatus::Staged);
        s.next();
        assert_eq!(s.cursor.current, Some(ids[1]));

        // Refresh with both lines shifted: second-chunk base content
        // moves, so its identity no longer matches. First chunk still
        // matches and carries the Staged status.
        s.refresh_files(vec![refresh_input(
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nKSHIFT\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nKNEW\n",
        )]);

        let cursor = s.cursor.current.expect("cursor present");
        assert_eq!(s.chunks[&cursor].status, ChunkStatus::Pending);
    }

    #[test]
    fn refresh_files_with_empty_input_clears_all_chunks() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");

        s.refresh_files(Vec::new());

        assert!(s.files.is_empty());
        assert!(s.chunks.is_empty());
        assert!(s.order.is_empty());
        assert_eq!(s.cursor.current, None);
    }

    #[test]
    fn refresh_file_replaces_one_file_entry_in_place() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        let total_before = s.order.len();
        let b_chunks_before: Vec<_> = s.files[1].chunks.clone();

        s.refresh_file(
            &PathBuf::from("a.txt"),
            refresh_input("a.txt", "a\nDIFFERENT\nc\n", "a\nNEWER\nc\n"),
        );

        assert_eq!(
            s.order.len(),
            total_before,
            "single-file refresh keeps the total chunk count",
        );
        assert_eq!(
            s.files[1].chunks, b_chunks_before,
            "the other file's chunk ids are untouched",
        );
        assert_eq!(s.files[0].buffer_text.as_str(), "a\nNEWER\nc\n");
    }

    #[test]
    fn refresh_file_preserves_decided_status_by_identity() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        s.set_status(ids[0], ChunkStatus::Staged);

        // Same base/buffer -> identity preserved -> Staged carries.
        s.refresh_file(
            &PathBuf::from("a.txt"),
            refresh_input("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n"),
        );

        let id = s.files[0].chunks[0];
        assert_eq!(s.chunks[&id].status, ChunkStatus::Staged);
    }

    #[test]
    fn refresh_file_keeps_cursor_on_matching_identity() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let cursor_before = s.cursor.current.expect("cursor set");
        let ident = s.identity_key(cursor_before).unwrap();

        s.refresh_file(
            &PathBuf::from("a.txt"),
            refresh_input("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n"),
        );

        let cursor_after = s.cursor.current.expect("cursor still present");
        assert_eq!(s.identity_key(cursor_after), Some(ident));
    }

    #[test]
    fn refresh_file_falls_back_cursor_to_first_chunk_in_refreshed_file() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        assert_eq!(s.cursor.current, Some(ids[0]));

        // Change the base content so the chunk's identity no longer
        // matches; cursor must land on the file's first new chunk.
        s.refresh_file(
            &PathBuf::from("a.txt"),
            refresh_input("a.txt", "a\nDIFFERENT\nc\n", "a\nNEWER\nc\n"),
        );

        let cursor = s.cursor.current.expect("cursor present");
        assert_eq!(cursor, s.files[0].chunks[0]);
    }

    #[test]
    fn refresh_file_with_unknown_path_is_noop() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let version_before = s.version;

        s.refresh_file(
            &PathBuf::from("nowhere.txt"),
            refresh_input("nowhere.txt", "x\n", "y\n"),
        );

        assert_eq!(s.version, version_before);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.order, ids);
    }

    #[test]
    fn refresh_file_on_other_file_leaves_cursor_alone() {
        let mut s = in_memory_session();
        let a_ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        // Cursor is on a.txt's first chunk by default.
        assert_eq!(s.cursor.current, Some(a_ids[0]));

        s.refresh_file(
            &PathBuf::from("b.txt"),
            refresh_input("b.txt", "x\nDIFFERENT\nz\n", "x\nNEWER\nz\n"),
        );

        assert_eq!(
            s.cursor.current,
            Some(a_ids[0]),
            "refreshing a different file must not move the cursor",
        );
    }

    fn chunk_with_move_provenance(
        left_prov: Option<MoveProvenance>,
        right_prov: Option<MoveProvenance>,
        right_present: bool,
    ) -> ReviewChunk {
        use crate::review::ReviewSide;
        let left = Some(ReviewSide {
            text: String::new(),
            line_num: 1,
            change_spans: Vec::new(),
            moved_spans: Vec::new(),
            move_provenance: left_prov,
        });
        let right = if right_present {
            Some(ReviewSide {
                text: String::new(),
                line_num: 1,
                change_spans: Vec::new(),
                moved_spans: Vec::new(),
                move_provenance: right_prov,
            })
        } else {
            None
        };
        ReviewChunk {
            id: ReviewChunkId(0),
            file_index: 0,
            chunk_index_in_file: 0,
            hunk: ReviewHunk {
                rows: vec![ReviewRow::Changed { left, right }],
            },
            buffer_line_range: 0..1,
            base_line_range: 0..1,
            buffer_byte_range: 0..0,
            base_byte_range: 0..0,
            status: ChunkStatus::Pending,
            approved: false,
        }
    }

    fn insert_synthetic_chunk(s: &mut ReviewSession, chunk: ReviewChunk) -> ReviewChunkId {
        let id = s.alloc_id();
        let mut chunk = chunk;
        chunk.id = id;
        s.chunks.insert(id, chunk);
        s.order.push(id);
        id
    }

    #[test]
    fn move_sources_in_chunk_returns_unique_right_provenances() {
        let mut s = in_memory_session();
        let prov_a = MoveProvenance {
            rel_path: "src/foo.rs".to_string(),
            line: 10,
        };
        let prov_b = MoveProvenance {
            rel_path: "src/bar.rs".to_string(),
            line: 20,
        };
        let id = insert_synthetic_chunk(
            &mut s,
            chunk_with_move_provenance(None, Some(prov_a.clone()), true),
        );

        // Add two more rows: one duplicate, one distinct.
        if let Some(chunk) = s.chunks.get_mut(&id) {
            use crate::review::ReviewSide;
            chunk.hunk.rows.push(ReviewRow::Changed {
                left: None,
                right: Some(ReviewSide {
                    text: String::new(),
                    line_num: 2,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: Some(prov_a.clone()),
                }),
            });
            chunk.hunk.rows.push(ReviewRow::Changed {
                left: None,
                right: Some(ReviewSide {
                    text: String::new(),
                    line_num: 3,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: Some(prov_b.clone()),
                }),
            });
        }

        assert_eq!(s.move_sources_in_chunk(id), vec![prov_a, prov_b]);
    }

    #[test]
    fn move_targets_in_chunk_returns_left_provenances_from_lhs_only_rows() {
        let mut s = in_memory_session();
        let prov = MoveProvenance {
            rel_path: "src/dest.rs".to_string(),
            line: 42,
        };
        // LHS-only row (right: None) with left.move_provenance set.
        let id = insert_synthetic_chunk(
            &mut s,
            chunk_with_move_provenance(Some(prov.clone()), None, false),
        );

        assert_eq!(s.move_targets_in_chunk(id), vec![prov]);
    }

    #[test]
    fn move_targets_in_chunk_skips_rows_with_right_side() {
        let mut s = in_memory_session();
        let prov = MoveProvenance {
            rel_path: "src/dest.rs".to_string(),
            line: 42,
        };
        // left.move_provenance is set but the row has a right side too,
        // so the row is not a target candidate.
        let id = insert_synthetic_chunk(&mut s, chunk_with_move_provenance(Some(prov), None, true));

        assert!(s.move_targets_in_chunk(id).is_empty());
    }

    #[test]
    fn move_sources_in_chunk_returns_empty_when_no_provenance() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        assert!(s.move_sources_in_chunk(ids[0]).is_empty());
    }

    #[test]
    fn chunk_for_buffer_line_finds_chunk_in_range() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        let chunk0_start = s.chunks[&ids[0]].buffer_line_range.start;
        let chunk1_start = s.chunks[&ids[1]].buffer_line_range.start;

        assert_eq!(s.chunk_for_buffer_line(0, chunk0_start), Some(ids[0]));
        assert_eq!(s.chunk_for_buffer_line(0, chunk1_start), Some(ids[1]));
    }

    #[test]
    fn chunk_for_buffer_line_falls_back_to_last_chunk_when_past_end() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "a.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        // Beyond every chunk's buffer_line_range -> last chunk.
        assert_eq!(
            s.chunk_for_buffer_line(0, 1_000),
            Some(*ids.last().unwrap())
        );
    }

    #[test]
    fn chunk_for_buffer_line_returns_none_when_file_has_no_chunks() {
        let s = in_memory_session();
        assert_eq!(s.chunk_for_buffer_line(0, 0), None);
    }

    #[test]
    fn collect_move_relationships_returns_empty_for_session_without_moves() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        assert!(s.collect_move_relationships().is_empty());
    }

    fn add_empty_file(s: &mut ReviewSession, rel_path: &str) {
        s.files.push(ReviewFile {
            path: PathBuf::from(rel_path),
            rel_path: rel_path.to_string(),
            language: None,
            base_text: Arc::new(String::new()),
            buffer_text: Arc::new(String::new()),
            chunks: Vec::new(),
        });
    }

    #[test]
    fn collect_move_relationships_dedupes_across_multi_row_moves() {
        use crate::review::ReviewSide;
        let mut s = in_memory_session();
        add_empty_file(&mut s, "src/dest.rs");
        let prov = MoveProvenance {
            rel_path: "src/origin.rs".to_string(),
            line: 10,
        };
        let id = insert_synthetic_chunk(
            &mut s,
            chunk_with_move_provenance(None, Some(prov.clone()), true),
        );
        // Add a second RHS row pointing at the same source --
        // simulates a 2-line moved hunk landing in this chunk.
        if let Some(chunk) = s.chunks.get_mut(&id) {
            chunk.hunk.rows.push(ReviewRow::Changed {
                left: None,
                right: Some(ReviewSide {
                    text: String::new(),
                    line_num: 2,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: Some(prov.clone()),
                }),
            });
        }

        let rels = s.collect_move_relationships();
        assert_eq!(rels.len(), 2, "distinct target lines -> distinct rels");
        assert_eq!(rels[0].source, prov);
        assert_eq!(rels[0].target.rel_path, "src/dest.rs");
        assert_eq!(rels[0].target.line, 0);
        assert_eq!(rels[1].target.line, 1);
    }

    #[test]
    fn collect_move_relationships_picks_up_lhs_only_provenances() {
        let mut s = in_memory_session();
        add_empty_file(&mut s, "src/origin.rs");
        let target = MoveProvenance {
            rel_path: "src/dest.rs".to_string(),
            line: 5,
        };
        let _id = insert_synthetic_chunk(
            &mut s,
            chunk_with_move_provenance(Some(target.clone()), None, false),
        );

        let rels = s.collect_move_relationships();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].source.rel_path, "src/origin.rs");
        assert_eq!(rels[0].source.line, 0);
        assert_eq!(rels[0].target, target);
    }

    use crate::test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER};

    #[test]
    fn snapshot_review_session_open() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.assert_snapshot("review_session_open");
    }

    #[test]
    fn snapshot_review_navigate_next() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("n");
        h.assert_snapshot("review_navigate_next");
    }

    #[test]
    fn snapshot_review_stage_current_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n");
        h.assert_snapshot("review_stage_current_chunk");
    }

    #[test]
    fn snapshot_review_unstage_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("u n");
        h.assert_snapshot("review_unstage_chunk");
    }

    #[test]
    fn review_space_approves_chunk_and_advances_cursor() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        let first = h.current_review_chunk_id();
        h.type_keys("Space");
        assert_eq!(
            h.chunk_status(first),
            ChunkStatus::Pending,
            "Space approves without changing staging status",
        );
        let after = h.current_review_chunk_id();
        assert_ne!(first, after, "cursor advanced to next chunk");
    }

    #[test]
    fn snapshot_review_skip_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("shift-S n");
        h.assert_snapshot("review_skip_chunk");
    }

    #[test]
    fn snapshot_review_progress_footer() {
        let mut h = TestHarness::with_size(120, 30);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n");
        h.assert_snapshot("review_progress_footer");
    }

    #[test]
    fn snapshot_review_complete_state() {
        let mut h = TestHarness::with_size(120, 30);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.type_keys("s n s");
        h.assert_snapshot("review_complete_state");
        assert!(h.with_review(|s| s.is_complete()));
        let has_badge = h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .is_some();
        assert!(has_badge, "complete review should surface a badge");
    }

    #[test]
    fn review_close_restores_normal_mode() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        assert_eq!(h.stoat.mode, "review");
        h.type_keys("q");
        assert_eq!(h.stoat.mode, "normal");
        assert!(h.stoat.active_workspace().review.is_none());
    }

    #[test]
    fn snapshot_review_multi_file_navigation() {
        let mut h = TestHarness::with_size(80, 20);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
            ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
        ]);
        h.type_keys("n");
        {
            let ws = h.stoat.active_workspace();
            let session = ws.review.as_ref().expect("session");
            let chunk = session.current().expect("current");
            assert_eq!(chunk.file_index, 1);
            assert_eq!(chunk.chunk_index_in_file, 0);
        }
        h.assert_snapshot("review_multi_file_navigation");
    }

    #[test]
    fn review_via_git_host_builds_session_from_working_tree() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session created by OpenReview");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.files[0].path, PathBuf::from("/work/a.rs"));
        assert_eq!(session.files[0].base_text.as_str(), REVIEW_TWO_HUNK_BASE);
        assert_eq!(
            session.files[0].buffer_text.as_str(),
            REVIEW_TWO_HUNK_BUFFER
        );
        assert_eq!(session.order.len(), 2);
        assert_eq!(h.stoat.mode, "review");
    }

    #[test]
    fn review_via_git_host_no_repo_is_noop() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.open_review();
        assert!(h.stoat.active_workspace().review.is_none());
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn review_refresh_via_git_carries_status() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRefresh);

        let statuses = h.with_review(|s| {
            s.order
                .iter()
                .map(|id| s.chunks.get(id).unwrap().status)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            statuses,
            vec![ChunkStatus::Staged, ChunkStatus::Pending],
            "first chunk's Staged decision should survive refresh; second should default to Pending",
        );
    }

    #[test]
    fn review_refresh_in_memory_carries_status() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRefresh);

        let statuses = h.with_review(|s| {
            s.order
                .iter()
                .map(|id| s.chunks.get(id).unwrap().status)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            statuses,
            vec![ChunkStatus::Staged, ChunkStatus::Pending],
            "InMemory refresh should re-derive hunks and carry decided statuses, not silently no-op",
        );
    }

    #[test]
    fn review_via_git_host_multi_file() {
        let mut h = TestHarness::with_size(80, 20);
        h.stage_review_scenario(
            "/work",
            &[
                ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
                ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
            ],
        );
        h.stoat.open_review();
        h.settle();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session");
        assert_eq!(session.files.len(), 2);
        assert_eq!(session.files[0].rel_path, "a.rs");
        assert_eq!(session.files[1].rel_path, "b.rs");
        assert!(session.order.len() >= 2);
    }

    #[test]
    fn stage_scenario_with_staged_seeds_both_buckets() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario_with_staged(
            "/work",
            &[("a.rs", "v1\n", "v2\n")],
            &[("b.rs", "staged\n")],
        );
        let repo =
            crate::host::GitHost::discover(&*h.fake_git, std::path::Path::new("/work")).unwrap();
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 2);
        let mut abs_paths: Vec<_> = changed.iter().map(|f| f.path.clone()).collect();
        abs_paths.sort();
        assert_eq!(abs_paths[0], PathBuf::from("/work/a.rs"));
        assert_eq!(abs_paths[1], PathBuf::from("/work/b.rs"));
    }

    #[test]
    fn open_agent_edit_review_via_helper_builds_session() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "", "added\n")]);
        let session = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .expect("session via helper");
        assert_eq!(session.files.len(), 2);
    }

    #[test]
    fn open_commit_review_via_helper_builds_session() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);
        h.open_commit_review("/work", "c2");
        let session = h.stoat.active_workspace().review.as_ref().unwrap();
        assert_eq!(session.files[0].buffer_text.as_str(), "v2\n");
    }

    #[test]
    fn scan_commit_builds_session_from_commit_vs_parent() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: PathBuf::from("/work"),
            sha: "c2".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for commit");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.files[0].base_text.as_str(), "v1\n");
        assert_eq!(session.files[0].buffer_text.as_str(), "v2\n");
        match &session.source {
            ReviewSource::Commit { sha, .. } => {
                assert_eq!(sha, "c2")
            },
            other => panic!("unexpected source: {other:?}"),
        }
    }

    #[test]
    fn scan_commit_root_diffs_against_empty_tree() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("root", &[("a.rs", "initial\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: PathBuf::from("/work"),
            sha: "root".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for root commit");
        assert_eq!(session.files[0].base_text.as_str(), "");
        assert_eq!(session.files[0].buffer_text.as_str(), "initial\n");
    }

    #[test]
    fn scan_commit_range_spans_multiple_commits() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n"), ("b.rs", "new\n")])
            .commit_with_parent(
                "c3",
                "c2",
                &[("a.rs", "v3\n"), ("b.rs", "new\n"), ("c.rs", "added\n")],
            );

        let action = stoat_action::OpenReviewCommitRange {
            workdir: PathBuf::from("/work"),
            from: "c1".into(),
            to: "c3".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for range");
        let rels: Vec<_> = session.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(rels.contains(&"a.rs"), "a.rs must be in range: {rels:?}");
        assert!(rels.contains(&"b.rs"), "b.rs must be in range: {rels:?}");
        assert!(rels.contains(&"c.rs"), "c.rs must be in range: {rels:?}");
    }

    #[test]
    fn scan_agent_edits_builds_session_without_repo() {
        use std::sync::Arc;
        let mut h = TestHarness::with_size(80, 14);
        let action = stoat_action::OpenReviewAgentEdits {
            edits: vec![
                stoat_action::AgentEdit {
                    path: PathBuf::from("/proposed/a.rs"),
                    base_text: Arc::new("old text\n".to_string()),
                    proposed_text: Arc::new("new text\n".to_string()),
                },
                stoat_action::AgentEdit {
                    path: PathBuf::from("/proposed/b.rs"),
                    base_text: Arc::new("".to_string()),
                    proposed_text: Arc::new("added\n".to_string()),
                },
            ],
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session for agent edits");
        assert_eq!(session.files.len(), 2);
        assert_eq!(session.files[0].base_text.as_str(), "old text\n");
        assert_eq!(session.files[0].buffer_text.as_str(), "new text\n");
        assert_eq!(session.files[1].base_text.as_str(), "");
        assert_eq!(session.files[1].buffer_text.as_str(), "added\n");
    }

    #[test]
    fn review_refresh_recomputes_commit_source() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\nline2\nline3\nline4\nline5\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "VX\nline2\nline3\nline4\nline5\n")]);

        let action = stoat_action::OpenReviewCommit {
            workdir: PathBuf::from("/work"),
            sha: "c2".into(),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);

        h.set_review_status(0, ChunkStatus::Staged);
        h.dispatch_review_refresh();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session survives refresh");
        assert_eq!(
            session.chunks[&session.order[0]].status,
            ChunkStatus::Staged
        );
    }

    #[test]
    fn fingerprint_stable_for_repeated_sessions() {
        let mut a = in_memory_session();
        let mut b = in_memory_session();
        let ids_a = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let ids_b = add(&mut b, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(ids_a.len(), 1);
        assert_eq!(ids_b.len(), 1);
        let fa = a.chunks[&ids_a[0]].fingerprint();
        let fb = b.chunks[&ids_b[0]].fingerprint();
        assert_eq!(fa, fb);
    }

    #[test]
    fn fingerprint_differs_for_different_buffer_text() {
        let mut a = in_memory_session();
        let mut b = in_memory_session();
        let ids_a = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let ids_b = add(&mut b, "x.txt", "a\nb\nc\n", "a\nQ\nc\n");
        let fa = a.chunks[&ids_a[0]].fingerprint();
        let fb = b.chunks[&ids_b[0]].fingerprint();
        assert_ne!(fa, fb);
    }

    #[test]
    fn fingerprint_differs_for_different_base_line_range() {
        let mut a = in_memory_session();
        let mut b = in_memory_session();
        add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        add(&mut b, "x.txt", "z\nz\nz\na\nb\nc\n", "z\nz\nz\na\nB\nc\n");
        let fa = a.chunks.values().next().expect("chunk").fingerprint();
        let fb = b.chunks.values().next().expect("chunk").fingerprint();
        assert_ne!(fa, fb);
    }

    #[test]
    fn snapshot_statuses_drops_pending() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        s.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        // ids[1] stays Pending
        let snap = s.snapshot_statuses();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.values().copied().next(), Some(ChunkStatus::Staged));
    }

    #[test]
    fn apply_statuses_restores_matching_chunks() {
        let mut a = in_memory_session();
        let ids = add(
            &mut a,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        a.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        a.chunks.get_mut(&ids[1]).unwrap().status = ChunkStatus::Skipped;
        let snap = a.snapshot_statuses();

        let mut b = in_memory_session();
        let new_ids = add(
            &mut b,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        let applied = b.apply_statuses(&snap);
        assert_eq!(applied, 2);
        assert_eq!(b.chunks[&new_ids[0]].status, ChunkStatus::Staged);
        assert_eq!(b.chunks[&new_ids[1]].status, ChunkStatus::Skipped);
    }

    #[test]
    fn apply_statuses_leaves_unmatched_chunks_pending() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        let snap = a.snapshot_statuses();

        let mut b = in_memory_session();
        let new_ids = add(&mut b, "x.txt", "a\nb\nc\n", "a\nQ\nc\n");
        let applied = b.apply_statuses(&snap);
        assert_eq!(applied, 0);
        assert_eq!(b.chunks[&new_ids[0]].status, ChunkStatus::Pending);
    }

    #[test]
    fn apply_statuses_bumps_version_when_any_chunk_updated() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        let snap = a.snapshot_statuses();

        let mut b = in_memory_session();
        add(&mut b, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let before = b.version;
        b.apply_statuses(&snap);
        assert!(b.version > before);
    }

    #[test]
    fn apply_statuses_skips_version_bump_when_status_unchanged() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        let snap = a.snapshot_statuses();

        let mut b = in_memory_session();
        let new_ids = add(&mut b, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        b.chunks.get_mut(&new_ids[0]).unwrap().status = ChunkStatus::Staged;
        let before = b.version;
        let applied = b.apply_statuses(&snap);
        assert_eq!(applied, 0);
        assert_eq!(b.version, before);
    }

    #[test]
    fn snapshot_approvals_drops_unapproved() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        assert_eq!(ids.len(), 2);
        s.chunks.get_mut(&ids[0]).unwrap().approved = true;
        let snap = s.snapshot_approvals();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.values().copied().next(), Some(true));
    }

    #[test]
    fn apply_approvals_restores_matching_chunks() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().approved = true;
        let snap = a.snapshot_approvals();

        let mut b = in_memory_session();
        let new_ids = add(&mut b, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let applied = b.apply_approvals(&snap);
        assert_eq!(applied, 1);
        assert!(b.chunks[&new_ids[0]].approved);
    }

    #[test]
    fn apply_approvals_leaves_unmatched_chunks_alone() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().approved = true;
        let snap = a.snapshot_approvals();

        let mut b = in_memory_session();
        let new_ids = add(&mut b, "x.txt", "a\nb\nc\n", "a\nQ\nc\n");
        let applied = b.apply_approvals(&snap);
        assert_eq!(applied, 0);
        assert!(!b.chunks[&new_ids[0]].approved);
    }

    #[test]
    fn apply_approvals_bumps_version_when_any_chunk_updated() {
        let mut a = in_memory_session();
        let ids = add(&mut a, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        a.chunks.get_mut(&ids[0]).unwrap().approved = true;
        let snap = a.snapshot_approvals();

        let mut b = in_memory_session();
        add(&mut b, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let before = b.version;
        b.apply_approvals(&snap);
        assert!(b.version > before);
    }

    #[test]
    fn set_approved_bumps_version_only_on_change() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        let before = s.version;
        s.set_approved(ids[0], true);
        assert!(s.chunks[&ids[0]].approved);
        assert!(s.version > before);

        let after_first = s.version;
        s.set_approved(ids[0], true);
        assert_eq!(s.version, after_first, "no version bump when unchanged");
    }

    #[test]
    fn next_unreviewed_walks_forward_then_wraps() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\nl\nm\nn\no\np\nq\nr\ns\nT\n",
        );
        assert!(ids.len() >= 3, "need at least 3 chunks");
        s.chunks.get_mut(&ids[1]).unwrap().approved = true;

        assert_eq!(s.cursor.current, Some(ids[0]));
        assert_eq!(s.next_unreviewed(), Some(ids[2]));
        assert_eq!(s.cursor.current, Some(ids[2]));

        let last = *ids.last().unwrap();
        if last != ids[2] {
            assert_eq!(s.next_unreviewed(), Some(last));
        }

        s.cursor.current = Some(last);
        let wrapped = s.next_unreviewed();
        assert_eq!(wrapped, Some(ids[0]), "wraps from end to first unapproved");
    }

    #[test]
    fn next_unreviewed_no_op_when_all_approved() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        for id in &ids {
            s.chunks.get_mut(id).unwrap().approved = true;
        }
        assert_eq!(s.next_unreviewed(), None);
    }

    #[test]
    fn next_unreviewed_no_op_on_empty_session() {
        let mut s = in_memory_session();
        assert_eq!(s.next_unreviewed(), None);
    }

    #[test]
    fn reset_progress_clears_status_and_approved_and_resets_cursor() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\nl\nm\nn\no\np\nq\nr\ns\nT\n",
        );
        assert!(ids.len() >= 2);
        for id in &ids {
            s.set_status(*id, ChunkStatus::Staged);
            s.set_approved(*id, true);
        }
        let last = *ids.last().unwrap();
        s.cursor.current = Some(last);
        let before = s.version;

        s.reset_progress();

        for id in &ids {
            assert_eq!(s.chunks[id].status, ChunkStatus::Pending);
            assert!(!s.chunks[id].approved);
        }
        assert_eq!(s.cursor.current, Some(ids[0]));
        assert!(s.version > before, "version bumps so observers refresh");
    }

    #[test]
    fn reset_cursor_snaps_back_to_first_chunk() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\nl\nm\nn\no\np\nq\nr\ns\nT\n",
        );
        assert!(ids.len() >= 2);
        let last = *ids.last().unwrap();
        s.cursor.current = Some(last);

        s.reset_cursor();
        assert_eq!(s.cursor.current, Some(ids[0]));
    }

    #[test]
    fn toggle_approved_flips_and_bumps_version() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "x.txt", "a\nb\nc\n", "a\nB\nc\n");
        assert!(!s.chunks[&ids[0]].approved);

        s.toggle_approved(ids[0]);
        assert!(s.chunks[&ids[0]].approved);

        s.toggle_approved(ids[0]);
        assert!(!s.chunks[&ids[0]].approved);
    }

    #[test]
    fn progress_counts_approved_independently() {
        let mut s = in_memory_session();
        let ids = add(
            &mut s,
            "x.txt",
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n",
        );
        s.chunks.get_mut(&ids[0]).unwrap().status = ChunkStatus::Staged;
        s.chunks.get_mut(&ids[0]).unwrap().approved = true;
        s.chunks.get_mut(&ids[1]).unwrap().approved = true;
        let p = s.progress();
        assert_eq!(p.staged, 1);
        assert_eq!(p.pending, 1);
        assert_eq!(p.approved, 2);
        assert_eq!(p.total, 2);
    }

    #[test]
    fn upsert_file_adds_new_file_and_settles_cursor() {
        let mut s = in_memory_session();
        assert!(s.files.is_empty());
        assert_eq!(s.cursor.current, None);

        let new_ids = s.upsert_file(refresh_input("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n"));

        assert_eq!(new_ids.len(), 1);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].chunks, new_ids);
        assert_eq!(s.order, new_ids);
        assert_eq!(s.cursor.current, Some(new_ids[0]));
    }

    #[test]
    fn upsert_file_replaces_existing_file_in_place() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        let total_before = s.order.len();
        let b_chunks_before: Vec<_> = s.files[1].chunks.clone();

        s.upsert_file(refresh_input("a.txt", "a\nDIFFERENT\nc\n", "a\nNEWER\nc\n"));

        assert_eq!(s.files.len(), 2);
        assert_eq!(s.order.len(), total_before);
        assert_eq!(s.files[1].chunks, b_chunks_before);
        assert_eq!(s.files[0].buffer_text.as_str(), "a\nNEWER\nc\n");
    }

    #[test]
    fn upsert_file_carries_decided_status_by_identity() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        s.set_status(ids[0], ChunkStatus::Staged);

        s.upsert_file(refresh_input("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n"));

        let id = s.files[0].chunks[0];
        assert_eq!(s.chunks[&id].status, ChunkStatus::Staged);
    }

    #[test]
    fn upsert_file_drops_entry_when_diff_becomes_empty() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let b_ids = add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        assert_eq!(s.chunks[&b_ids[0]].file_index, 1);

        s.upsert_file(refresh_input("a.txt", "same\n", "same\n"));

        assert_eq!(s.files.len(), 1, "a.txt entry dropped");
        assert_eq!(s.files[0].path, PathBuf::from("b.txt"));
        assert_eq!(
            s.chunks[&b_ids[0]].file_index, 0,
            "later file's chunk file_index shifted down",
        );
        assert_eq!(s.order, b_ids);
    }

    #[test]
    fn upsert_file_empty_diff_for_unknown_path_is_noop() {
        let mut s = in_memory_session();
        let ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let version_before = s.version;

        let new_ids = s.upsert_file(refresh_input("nowhere.txt", "x\n", "x\n"));

        assert!(new_ids.is_empty());
        assert_eq!(s.version, version_before);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.order, ids);
    }

    #[test]
    fn upsert_file_leaves_cursor_on_unrelated_file() {
        let mut s = in_memory_session();
        let a_ids = add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        assert_eq!(s.cursor.current, Some(a_ids[0]));

        s.upsert_file(refresh_input("b.txt", "x\nDIFFERENT\nz\n", "x\nNEWER\nz\n"));

        assert_eq!(s.cursor.current, Some(a_ids[0]));
    }

    #[test]
    fn upsert_file_drop_falls_back_cursor_when_on_dropped_file() {
        let mut s = in_memory_session();
        add(&mut s, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let b_ids = add(&mut s, "b.txt", "x\nOLD\nz\n", "x\nNEW\nz\n");
        let a_id = s.files[0].chunks[0];
        s.cursor.current = Some(a_id);

        s.upsert_file(refresh_input("a.txt", "same\n", "same\n"));

        assert_eq!(s.cursor.current, Some(b_ids[0]));
    }
}
