use crate::{
    editor_state::EditorId,
    review::{extract_review_hunks, ReviewHunk, ReviewRow},
};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use stoat_language::Language;

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

/// Provenance of the content under review. Only `WorkingTree` and `InMemory`
/// resolve today; the other variants are slots reserved for future work
/// (commit review, commit-range stepping, agent-proposed edits) and are
/// matched at the resolver layer, which returns `NotImplemented` for them.
#[derive(Clone, Debug)]
pub(crate) enum ReviewSource {
    WorkingTree {
        workdir: PathBuf,
    },
    #[allow(dead_code)]
    Commit {
        workdir: PathBuf,
        sha: String,
    },
    #[allow(dead_code)]
    CommitRange {
        workdir: PathBuf,
        from: String,
        to: String,
    },
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

/// UI-facing cache derived from a [`ReviewSession`]. Attached to an editor
/// so `render_review` can paint without walking the session on every frame,
/// and so navigation handlers can map a chunk id to a display row.
#[derive(Clone, Debug)]
pub(crate) struct ReviewViewState {
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
    pub(crate) fn from_session(session: &ReviewSession) -> Self {
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
    pub(crate) fn refresh_from_session(&mut self, session: &ReviewSession) {
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
    pub(crate) fn chunk_and_status_at_row(&self, row: u32) -> Option<(ReviewChunkId, ChunkStatus)> {
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
    pub(crate) fn row_of_chunk(&self, id: ReviewChunkId) -> Option<u32> {
        self.chunk_row_starts
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, r)| *r)
    }
}

pub(crate) struct ReviewSession {
    pub source: ReviewSource,
    pub files: Vec<ReviewFile>,
    pub chunks: HashMap<ReviewChunkId, ReviewChunk>,
    pub order: Vec<ReviewChunkId>,
    pub cursor: ReviewCursor,
    pub view_editor: Option<EditorId>,
    /// Bumped on any mutation so editor-level caches can detect staleness.
    pub version: u64,
    next_id: u32,
}

impl ReviewSession {
    pub(crate) fn new(source: ReviewSource) -> Self {
        Self {
            source,
            files: Vec::new(),
            chunks: HashMap::new(),
            order: Vec::new(),
            cursor: ReviewCursor::default(),
            view_editor: None,
            version: 0,
            next_id: 0,
        }
    }

    /// Parse `base_text` against `buffer_text` with the given language and
    /// append one [`ReviewFile`] plus its chunks to the session. Returns
    /// the ids of the chunks that were added, in visit order. Files that
    /// produce no hunks are still recorded so that indexing stays stable.
    pub(crate) fn add_file(
        &mut self,
        path: PathBuf,
        rel_path: String,
        language: Option<Arc<Language>>,
        base_text: Arc<String>,
        buffer_text: Arc<String>,
    ) -> Vec<ReviewChunkId> {
        let hunks = extract_review_hunks(language.as_ref(), &base_text, &buffer_text, 3);
        let file_index = self.files.len();

        let base_offsets = line_byte_offsets(&base_text);
        let buffer_offsets = line_byte_offsets(&buffer_text);

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
                },
            );
            self.order.push(id);
            chunk_ids.push(id);
        }

        self.files.push(ReviewFile {
            path,
            rel_path,
            language,
            base_text,
            buffer_text,
            chunks: chunk_ids.clone(),
        });

        if self.cursor.current.is_none() {
            self.cursor.current = self.order.first().copied();
        }

        self.version += 1;
        chunk_ids
    }

    #[allow(dead_code)]
    pub(crate) fn chunk(&self, id: ReviewChunkId) -> Option<&ReviewChunk> {
        self.chunks.get(&id)
    }

    #[allow(dead_code)]
    pub(crate) fn current(&self) -> Option<&ReviewChunk> {
        self.cursor.current.and_then(|id| self.chunks.get(&id))
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
        if let Some(chunk) = self.chunks.get_mut(&id) {
            chunk.status = status;
            self.version += 1;
        }
    }

    /// Toggle between `Staged` and `Unstaged` for the given chunk. Chunks
    /// currently in `Pending` or `Skipped` flip to `Staged`, giving users
    /// a one-key path from "not looked at" into the accept lane.
    pub(crate) fn toggle_stage(&mut self, id: ReviewChunkId) {
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

    pub(crate) fn progress(&self) -> ReviewProgress {
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
            }
        }
        p
    }

    pub(crate) fn is_complete(&self) -> bool {
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
    pub(crate) fn identity_key(&self, id: ReviewChunkId) -> Option<ChunkIdentity> {
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
pub(crate) struct ChunkIdentity {
    pub path: PathBuf,
    pub base_line_start: u32,
    pub base_line_end: u32,
    pub content_hash: u64,
}

fn line_byte_offsets(text: &str) -> Vec<(usize, usize)> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut offsets = Vec::new();
    let mut pos = 0usize;
    for line in text.split('\n') {
        let end = pos + line.len();
        offsets.push((pos, end));
        pos = end + 1;
    }
    if text.ends_with('\n') && !offsets.is_empty() {
        offsets.pop();
    }
    offsets
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
}
