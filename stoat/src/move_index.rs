//! Workspace-wide content-id index for cross-file move detection.
//!
//! The per-diff [`stoat_language::structural_diff::find_moves`] pass
//! only sees one pair of syntax arenas at a time. Moves that cross
//! files in the same changeset (e.g. a function relocated from
//! `old.rs` to `new.rs`) need a workspace-level primitive that
//! records every parsed buffer's [`ContentId`] and returns candidate
//! source locations from other buffers.
//!
//! The index stores one entry per candidate node and is rebuilt per
//! buffer version. The rebuild is O(N) in arena size and runs on the
//! main thread next to the parse step, so the cost is bounded by the
//! same budget that gates parse jobs.
//!
//! ### Data shape
//!
//! Each [`ContentId`] maps to a `Vec` of [`IndexedNode`]s describing
//! every buffer+node that shares that id. The node records only the
//! minimum information needed to emit a [`MoveSource`] downstream:
//! the buffer's [`BufferRef`] identity, the node's byte range in the
//! source text, and the derived line range.
//!
//! ### Query
//!
//! [`MoveIndex::candidates_for`] takes a [`ContentId`] and an optional
//! excluded [`BufferRef`] (the "local" buffer whose own nodes should
//! be filtered out) and returns every matching node. The diff pipeline
//! uses it to augment the per-file candidate list inside
//! `find_moves`: a local Pending node whose [`ContentId`] has no
//! intra-file counterpart but has cross-buffer hits is emitted as a
//! `Moved` [`stoat_language::structural_diff::DiffChange`] whose
//! [`MoveMetadata`]`.sources` list contains those cross-file
//! candidates (complete with their `MoveSource::buffer` populated).

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_language::structural_diff::{BufferRef, ContentId};

/// One candidate node: the buffer it lives in and where in the source
/// text it sits. Consumers convert these to
/// [`stoat_language::structural_diff::MoveSource`]s by tagging with
/// the appropriate [`stoat_language::structural_diff::Side`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedNode {
    pub(crate) buffer: BufferRef,
    pub(crate) byte_range: Range<usize>,
    pub(crate) line_range: Range<u32>,
}

/// Workspace-wide `ContentId` -> candidate nodes mapping, plus a
/// per-buffer version stamp so callers can detect staleness cheaply.
#[derive(Default)]
pub(crate) struct MoveIndex {
    by_cid: HashMap<ContentId, Vec<IndexedNode>>,
    per_buffer_version: HashMap<PathBuf, u64>,
    fingerprints: HashMap<PathBuf, [u8; 32]>,
}

impl MoveIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the version this index last recorded for `path`, or
    /// `None` if the path has never been indexed.
    pub(crate) fn version_for(&self, path: &Path) -> Option<u64> {
        self.per_buffer_version.get(path).copied()
    }

    /// Return every indexed candidate for `cid`, optionally excluding
    /// nodes that belong to `exclude` (usually the "local" buffer that
    /// the caller is already pairing against within its own arena).
    pub(crate) fn candidates_for<'a>(
        &'a self,
        cid: ContentId,
        exclude: Option<&'a BufferRef>,
    ) -> impl Iterator<Item = &'a IndexedNode> + 'a {
        self.by_cid
            .get(&cid)
            .into_iter()
            .flat_map(|v| v.iter())
            .filter(move |node| match exclude {
                Some(buf) => node.buffer != *buf,
                None => true,
            })
    }

    /// Rebuild the index entries owned by `path` from a freshly-walked
    /// candidate list. Previous entries for this buffer are dropped
    /// before the new ones are inserted, so the rebuild is idempotent
    /// for a given version.
    pub(crate) fn rebuild_buffer(
        &mut self,
        path: PathBuf,
        fingerprint: [u8; 32],
        version: u64,
        candidates: impl IntoIterator<Item = (ContentId, Range<usize>, Range<u32>)>,
    ) {
        self.drop_buffer_entries(&path);

        let buffer_ref = BufferRef {
            path: path.clone(),
            fingerprint,
        };
        for (cid, byte_range, line_range) in candidates {
            let entry = IndexedNode {
                buffer: buffer_ref.clone(),
                byte_range,
                line_range,
            };
            self.by_cid.entry(cid).or_default().push(entry);
        }

        self.per_buffer_version.insert(path.clone(), version);
        self.fingerprints.insert(path, fingerprint);
    }

    /// Forget every indexed node and metadata slot for `path`. Called
    /// before a rebuild or when the buffer is closed.
    pub(crate) fn drop_buffer(&mut self, path: &Path) {
        self.drop_buffer_entries(path);
        self.per_buffer_version.remove(path);
        self.fingerprints.remove(path);
    }

    fn drop_buffer_entries(&mut self, path: &Path) {
        // O(total_entries). Acceptable: index sizes are bounded by
        // file count + atom count per file, and rebuilds run at
        // parse-job cadence (not per keystroke).
        for nodes in self.by_cid.values_mut() {
            nodes.retain(|node| node.buffer.path != path);
        }
        self.by_cid.retain(|_, nodes| !nodes.is_empty());
    }

    /// Number of buffers currently represented in the index. Mostly
    /// for observability in tests.
    #[allow(dead_code)]
    pub(crate) fn buffer_count(&self) -> usize {
        self.per_buffer_version.len()
    }

    /// Total candidate nodes across every buffer.
    #[allow(dead_code)]
    pub(crate) fn total_entries(&self) -> usize {
        self.by_cid.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(path: &str, byte: u64) -> BufferRef {
        let mut fp = [0u8; 32];
        fp[0..8].copy_from_slice(&byte.to_le_bytes());
        BufferRef {
            path: PathBuf::from(path),
            fingerprint: fp,
        }
    }

    fn cid(n: u64) -> ContentId {
        ContentId(n)
    }

    #[test]
    fn rebuild_and_query_basics() {
        let mut idx = MoveIndex::new();
        idx.rebuild_buffer(
            PathBuf::from("a.rs"),
            [1u8; 32],
            1,
            vec![(cid(100), 0..10, 0..1), (cid(200), 10..20, 1..2)],
        );
        idx.rebuild_buffer(
            PathBuf::from("b.rs"),
            [2u8; 32],
            3,
            vec![(cid(100), 5..15, 0..1)],
        );

        // cid(100) exists in both buffers.
        let hits: Vec<_> = idx.candidates_for(cid(100), None).collect();
        assert_eq!(hits.len(), 2);
        // cid(200) only in a.rs.
        let hits: Vec<_> = idx.candidates_for(cid(200), None).collect();
        assert_eq!(hits.len(), 1);
        // Unknown cid returns nothing.
        let hits: Vec<_> = idx.candidates_for(cid(999), None).collect();
        assert!(hits.is_empty());
    }

    #[test]
    fn exclude_filters_local_buffer() {
        let mut idx = MoveIndex::new();
        let a = buf("a.rs", 1);
        let b = buf("b.rs", 2);
        idx.rebuild_buffer(a.path.clone(), a.fingerprint, 1, vec![(cid(1), 0..5, 0..1)]);
        idx.rebuild_buffer(b.path.clone(), b.fingerprint, 1, vec![(cid(1), 0..5, 0..1)]);

        let cross_only: Vec<_> = idx.candidates_for(cid(1), Some(&a)).collect();
        assert_eq!(cross_only.len(), 1);
        assert_eq!(cross_only[0].buffer.path, PathBuf::from("b.rs"));
    }

    #[test]
    fn rebuild_replaces_prior_entries() {
        let mut idx = MoveIndex::new();
        idx.rebuild_buffer(
            PathBuf::from("a.rs"),
            [1u8; 32],
            1,
            vec![(cid(100), 0..10, 0..1)],
        );
        assert_eq!(idx.total_entries(), 1);

        // Rebuild with different entries.
        idx.rebuild_buffer(
            PathBuf::from("a.rs"),
            [1u8; 32],
            2,
            vec![(cid(200), 0..10, 0..1), (cid(300), 10..20, 1..2)],
        );
        assert_eq!(idx.total_entries(), 2);
        assert!(idx.candidates_for(cid(100), None).next().is_none());
        assert_eq!(idx.version_for(Path::new("a.rs")), Some(2));
    }

    #[test]
    fn drop_buffer_removes_every_entry() {
        let mut idx = MoveIndex::new();
        idx.rebuild_buffer(
            PathBuf::from("a.rs"),
            [1u8; 32],
            1,
            vec![(cid(1), 0..5, 0..1), (cid(2), 5..10, 0..1)],
        );
        idx.rebuild_buffer(
            PathBuf::from("b.rs"),
            [2u8; 32],
            1,
            vec![(cid(1), 0..5, 0..1)],
        );
        assert_eq!(idx.buffer_count(), 2);

        idx.drop_buffer(Path::new("a.rs"));
        assert_eq!(idx.buffer_count(), 1);
        assert_eq!(idx.candidates_for(cid(1), None).count(), 1);
        assert_eq!(idx.candidates_for(cid(2), None).count(), 0);
        assert!(idx.version_for(Path::new("a.rs")).is_none());
    }
}
