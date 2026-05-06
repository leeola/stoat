use crate::buffer::{BufferHistory, BufferId, SharedBuffer, TextBuffer};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use stoat_language::{
    drop_syntax_in_background, structural_diff::DiffResult, Language, SyntaxMap, SyntaxState,
};

/// Memoized [`DiffResult`] for a `(buffer, base_text)` pair. Cached
/// on [`BufferRegistry`] so repeat review-view renders and consumer
/// queries do not rerun the full structural-diff pipeline. Keyed
/// on the buffer version that was diffed and a blake3 fingerprint
/// of the base text: if either changes, the cache entry is stale.
#[derive(Clone)]
pub(crate) struct CachedDiff {
    pub buffer_version: u64,
    pub base_fingerprint: [u8; 32],
    pub result: Arc<DiffResult>,
}

/// One entry surfaced by [`BufferRegistry::dirty_buffers`]. `path` is
/// `Some` for file-backed buffers and `None` for scratch buffers.
#[derive(Clone, Debug)]
pub(crate) struct DirtyBuffer {
    pub id: BufferId,
    pub path: Option<PathBuf>,
}

#[allow(dead_code)]
struct BufferEntry {
    buffer: SharedBuffer,
    path: Option<PathBuf>,
    language: Option<Arc<Language>>,
    syntax: Option<SyntaxState>,
    /// Multi-layer syntax storage. Populated alongside [`Self::syntax`]
    /// so the legacy single-tree highlight path keeps working while
    /// callers migrate to capture merging. The `parse_buffer_step`
    /// pipeline writes to both fields on every reparse.
    syntax_map: Option<SyntaxMap>,
    diff: Option<CachedDiff>,
}

pub(crate) struct BufferRegistry {
    buffers: HashMap<BufferId, BufferEntry>,
    path_to_id: HashMap<PathBuf, BufferId>,
    next_id: u64,
}

impl BufferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            path_to_id: HashMap::new(),
            next_id: 1,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.buffers.len()
    }

    /// True when the registry holds exactly one buffer, that buffer has no
    /// backing file path, and its text is empty: the state left by
    /// [`Self::new_scratch`] without any subsequent edits. Used by
    /// [`crate::workspace::Workspace::is_fresh`] to decide whether a
    /// workspace is worth persisting.
    pub(crate) fn only_empty_scratch(&self) -> bool {
        if self.buffers.len() != 1 || !self.path_to_id.is_empty() {
            return false;
        }
        let Some(entry) = self.buffers.values().next() else {
            return false;
        };
        if entry.path.is_some() {
            return false;
        }
        entry
            .buffer
            .read()
            .expect("buffer poisoned")
            .snapshot
            .is_empty()
    }

    fn allocate_id(&mut self) -> BufferId {
        let id = BufferId::new(self.next_id);
        self.next_id += 1;
        id
    }

    pub(crate) fn new_scratch(&mut self) -> (BufferId, SharedBuffer) {
        let id = self.allocate_id();
        let buffer = Arc::new(RwLock::new(TextBuffer::new(id)));
        self.buffers.insert(
            id,
            BufferEntry {
                buffer: buffer.clone(),
                path: None,
                language: None,
                syntax: None,
                syntax_map: None,
                diff: None,
            },
        );
        (id, buffer)
    }

    /// Returns the existing buffer for `path`, or creates one with `text`.
    /// If the buffer already exists, `text` is ignored.
    pub(crate) fn open(&mut self, path: &Path, text: &str) -> (BufferId, SharedBuffer) {
        if let Some(&id) = self.path_to_id.get(path) {
            let entry = &self.buffers[&id];
            return (id, entry.buffer.clone());
        }

        let id = self.allocate_id();
        let buffer = Arc::new(RwLock::new(TextBuffer::with_text(id, text)));
        let path_buf = path.to_path_buf();
        self.path_to_id.insert(path_buf.clone(), id);
        self.buffers.insert(
            id,
            BufferEntry {
                buffer: buffer.clone(),
                path: Some(path_buf),
                language: None,
                syntax: None,
                syntax_map: None,
                diff: None,
            },
        );
        (id, buffer)
    }

    pub(crate) fn get(&self, id: BufferId) -> Option<SharedBuffer> {
        self.buffers.get(&id).map(|e| e.buffer.clone())
    }

    pub(crate) fn id_for_path(&self, path: &Path) -> Option<BufferId> {
        self.path_to_id.get(path).copied()
    }

    /// Drop `id` from the registry. Removes the path-to-id mapping
    /// when the entry was path-bound and returns that path so the
    /// caller can build an LSP URI for `did_close`. Returns `None`
    /// when the buffer was scratch (or unknown).
    pub(crate) fn remove(&mut self, id: BufferId) -> Option<PathBuf> {
        let entry = self.buffers.remove(&id)?;
        let path = entry.path?;
        self.path_to_id.remove(&path);
        Some(path)
    }

    /// Updates the path of an open buffer in place. No-op when `old` has no
    /// open buffer. Returns `true` if a remapping happened. Used by
    /// `WorkspaceEdit::Rename` so an open buffer for the renamed file
    /// stays addressable by its new path.
    pub(crate) fn rename_path(&mut self, old: &Path, new: &Path) -> bool {
        let Some(id) = self.path_to_id.remove(old) else {
            return false;
        };
        self.path_to_id.insert(new.to_path_buf(), id);
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.path = Some(new.to_path_buf());
        }
        true
    }

    #[allow(dead_code)]
    pub(crate) fn path_for(&self, id: BufferId) -> Option<&Path> {
        self.buffers.get(&id).and_then(|e| e.path.as_deref())
    }

    /// Returns paths of currently-open path-bound buffers in lexicographic
    /// order. Scratch buffers (with no path) are skipped. The deterministic
    /// ordering matches what the file finder shows for the All scope.
    pub(crate) fn open_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.path_to_id.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Every buffer whose `dirty` flag is set: path-bound first sorted by
    /// path, scratch buffers after sorted by id. Used by `QuitAll` to drive
    /// the unsaved-buffers confirmation modal.
    pub(crate) fn dirty_buffers(&self) -> Vec<DirtyBuffer> {
        let mut out: Vec<DirtyBuffer> = self
            .buffers
            .iter()
            .filter(|(_, entry)| entry.buffer.read().expect("buffer poisoned").dirty)
            .map(|(id, entry)| DirtyBuffer {
                id: *id,
                path: entry.path.clone(),
            })
            .collect();
        out.sort_by(|a, b| match (&a.path, &b.path) {
            (Some(ap), Some(bp)) => ap.cmp(bp),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        out
    }

    pub(crate) fn language_for(&self, id: BufferId) -> Option<Arc<Language>> {
        self.buffers.get(&id)?.language.clone()
    }

    pub(crate) fn set_language(&mut self, id: BufferId, lang: Arc<Language>) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.language = Some(lang);
            entry.syntax = None;
            entry.syntax_map = None;
        }
    }

    pub(crate) fn syntax_version(&self, id: BufferId) -> Option<u64> {
        self.buffers.get(&id)?.syntax.as_ref().map(|s| s.version)
    }

    pub(crate) fn store_syntax(&mut self, id: BufferId, state: SyntaxState) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            // Send the displaced state to a background drainer so its
            // potentially-large tree-sitter tree drops off the main thread.
            if let Some(prev) = entry.syntax.replace(state) {
                drop_syntax_in_background(prev);
            }
        }
    }

    /// Move the prior [`SyntaxState`] out of the registry. The caller is
    /// expected to update it (`tree.edit` + reparse) and put it back via
    /// [`Self::store_syntax`]. Returns `None` if no state has been stored.
    pub(crate) fn take_syntax(&mut self, id: BufferId) -> Option<SyntaxState> {
        self.buffers.get_mut(&id)?.syntax.take()
    }

    /// Borrow the multi-layer [`SyntaxMap`] for `id`, if one has been
    /// installed by the parse pipeline. Used by the capture-merging
    /// highlight path.
    #[allow(dead_code)]
    pub(crate) fn syntax_map(&self, id: BufferId) -> Option<&SyntaxMap> {
        self.buffers.get(&id)?.syntax_map.as_ref()
    }

    /// Replace the multi-layer [`SyntaxMap`] for `id`. Called by
    /// `parse_buffer_step` after each successful reparse so the
    /// capture-merging consumers always see the latest layer set.
    pub(crate) fn store_syntax_map(&mut self, id: BufferId, map: SyntaxMap) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.syntax_map = Some(map);
        }
    }

    /// Move the multi-layer [`SyntaxMap`] for `id` out of the
    /// registry, so the next reparse can interpolate it incrementally
    /// before reinstalling.
    pub(crate) fn take_syntax_map(&mut self, id: BufferId) -> Option<SyntaxMap> {
        self.buffers.get_mut(&id)?.syntax_map.take()
    }

    /// Return a cached [`DiffResult`] for `(buffer, base_text)` if one
    /// was stored against the current buffer version and base
    /// fingerprint; otherwise `None`. Callers recompute and cache via
    /// [`Self::store_diff`] on miss.
    #[allow(dead_code)]
    pub(crate) fn cached_diff(
        &self,
        id: BufferId,
        buffer_version: u64,
        base_fingerprint: [u8; 32],
    ) -> Option<Arc<DiffResult>> {
        let entry = self.buffers.get(&id)?.diff.as_ref()?;
        if entry.buffer_version == buffer_version && entry.base_fingerprint == base_fingerprint {
            Some(entry.result.clone())
        } else {
            None
        }
    }

    /// Store a newly-computed [`DiffResult`] for `id`. Supersedes any
    /// prior cache entry regardless of version/fingerprint; callers
    /// that want stale detection should check [`Self::cached_diff`]
    /// before recomputing.
    #[allow(dead_code)]
    pub(crate) fn store_diff(
        &mut self,
        id: BufferId,
        buffer_version: u64,
        base_fingerprint: [u8; 32],
        result: Arc<DiffResult>,
    ) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.diff = Some(CachedDiff {
                buffer_version,
                base_fingerprint,
                result,
            });
        }
    }

    /// Drop any cached diff for `id`. Call when the buffer's base
    /// text changes or when the buffer is closed.
    #[allow(dead_code)]
    pub(crate) fn invalidate_diff(&mut self, id: BufferId) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.diff = None;
        }
    }

    /// Capture the registry state for persistence. Each entry carries its
    /// full [`BufferHistory`] so replay on restore reconstructs identical
    /// fragment trees and anchors. Scratch buffers (no path) are included so
    /// their edit history also round-trips.
    pub(crate) fn snapshot(&self) -> BufferRegistrySnapshot {
        let mut entries: Vec<BufferEntrySnap> = self
            .buffers
            .iter()
            .map(|(id, entry)| BufferEntrySnap {
                id: *id,
                path: entry.path.clone(),
                history: {
                    let guard = entry.buffer.read().expect("buffer poisoned");
                    guard.history()
                },
            })
            .collect();
        entries.sort_by_key(|e| e.id);
        BufferRegistrySnapshot {
            entries,
            next_id: self.next_id,
        }
    }

    /// Rehydrate a registry from a [`BufferRegistrySnapshot`]. For each entry
    /// the saved [`BufferHistory`] is replayed on a fresh buffer, which
    /// reconstructs the fragment tree, undo stack, and dirty state exactly as
    /// they were at save time. The on-disk file is not read: if it has drifted
    /// we'd have to choose between it and the saved edits, and the saved edits
    /// win unconditionally since persistence represents the user's explicit
    /// last-known state.
    pub(crate) fn restore_from(&mut self, snap: BufferRegistrySnapshot) {
        self.buffers.clear();
        self.path_to_id.clear();
        self.next_id = snap.next_id.max(1);

        for entry in snap.entries {
            let buffer = Arc::new(RwLock::new(TextBuffer::from_history(
                entry.id,
                &entry.history,
            )));
            if let Some(path) = entry.path.as_ref() {
                self.path_to_id.insert(path.clone(), entry.id);
            }
            self.buffers.insert(
                entry.id,
                BufferEntry {
                    buffer,
                    path: entry.path,
                    language: None,
                    syntax: None,
                    syntax_map: None,
                    diff: None,
                },
            );
        }
    }
}

/// Serializable view of [`BufferRegistry`]. Each entry carries its
/// [`BufferHistory`] (the replayable op log) so restoration reconstructs the
/// fragment tree, anchors, undo stack, and dirty state exactly. Syntax and
/// diff caches are regenerable and deliberately not persisted.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BufferRegistrySnapshot {
    pub entries: Vec<BufferEntrySnap>,
    pub next_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BufferEntrySnap {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub history: BufferHistory,
}

/// 32-byte blake3 hash of `text`. Used both to key [`CachedDiff`] in
/// the buffer registry and to populate
/// [`stoat_language::structural_diff::BufferRef::fingerprint`] for
/// cross-file move detection in the structural diff pipeline.
#[allow(dead_code)]
pub(crate) fn fingerprint_bytes(text: &str) -> [u8; 32] {
    blake3::hash(text.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_generates_unique_ids() {
        let mut reg = BufferRegistry::new();
        let (id1, _) = reg.new_scratch();
        let (id2, _) = reg.new_scratch();
        assert_ne!(id1, id2);
    }

    #[test]
    fn open_deduplicates_by_path() {
        let mut reg = BufferRegistry::new();
        let (id1, buf1) = reg.open(Path::new("/a.txt"), "hello");
        let (id2, buf2) = reg.open(Path::new("/a.txt"), "ignored");
        assert_eq!(id1, id2);
        assert!(Arc::ptr_eq(&buf1, &buf2));
        let guard = buf1.read().unwrap();
        assert_eq!(guard.rope().to_string(), "hello");
    }

    #[test]
    fn open_different_paths() {
        let mut reg = BufferRegistry::new();
        let (id1, _) = reg.open(Path::new("/a.txt"), "a");
        let (id2, _) = reg.open(Path::new("/b.txt"), "b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn get_existing() {
        let mut reg = BufferRegistry::new();
        let (id, original) = reg.new_scratch();
        let fetched = reg.get(id).unwrap();
        assert!(Arc::ptr_eq(&original, &fetched));
    }

    #[test]
    fn get_nonexistent() {
        let reg = BufferRegistry::new();
        assert!(reg.get(BufferId::new(999)).is_none());
    }

    #[test]
    fn path_for_scratch_is_none() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        assert!(reg.path_for(id).is_none());
    }

    #[test]
    fn path_for_file_buffer() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.open(Path::new("/foo/bar.rs"), "");
        assert_eq!(reg.path_for(id), Some(Path::new("/foo/bar.rs")));
    }

    #[test]
    fn diff_cache_hits_on_matching_version_and_fingerprint() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let result = Arc::new(DiffResult::default());
        let fp = fingerprint_bytes("base text");
        reg.store_diff(id, 7, fp, result.clone());
        let hit = reg.cached_diff(id, 7, fp).expect("cache hit");
        assert!(Arc::ptr_eq(&hit, &result));
    }

    #[test]
    fn diff_cache_miss_on_version_change() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp = fingerprint_bytes("base");
        reg.store_diff(id, 1, fp, Arc::new(DiffResult::default()));
        assert!(reg.cached_diff(id, 2, fp).is_none());
    }

    #[test]
    fn diff_cache_miss_on_fingerprint_change() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp1 = fingerprint_bytes("one");
        let fp2 = fingerprint_bytes("two");
        reg.store_diff(id, 1, fp1, Arc::new(DiffResult::default()));
        assert!(reg.cached_diff(id, 1, fp2).is_none());
    }

    #[test]
    fn diff_cache_invalidate_clears_entry() {
        let mut reg = BufferRegistry::new();
        let (id, _) = reg.new_scratch();
        let fp = fingerprint_bytes("x");
        reg.store_diff(id, 1, fp, Arc::new(DiffResult::default()));
        reg.invalidate_diff(id);
        assert!(reg.cached_diff(id, 1, fp).is_none());
    }

    #[test]
    fn fingerprint_differs_per_text() {
        assert_ne!(fingerprint_bytes("a"), fingerprint_bytes("b"));
        assert_eq!(fingerprint_bytes("abc"), fingerprint_bytes("abc"));
    }
}
