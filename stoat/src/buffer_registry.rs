use crate::buffer::{BufferId, SharedBuffer, TextBuffer};
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

    #[allow(dead_code)]
    pub(crate) fn path_for(&self, id: BufferId) -> Option<&Path> {
        self.buffers.get(&id).and_then(|e| e.path.as_deref())
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
}

/// Compute a blake3-style 32-byte fingerprint of `text` suitable for
/// keying [`CachedDiff`]. Implemented via the standard library's
/// [`DefaultHasher`] chained twice for 32 bytes; the crate has no
/// dependency on a cryptographic hash and the key only needs to
/// distinguish distinct base texts, not resist adversarial collision.
#[allow(dead_code)]
pub(crate) fn fingerprint_bytes(text: &str) -> [u8; 32] {
    use std::hash::{Hash, Hasher};
    let mut out = [0u8; 32];
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h1);
    let a = h1.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    0xAAu8.hash(&mut h2);
    text.hash(&mut h2);
    let b = h2.finish();
    let mut h3 = std::collections::hash_map::DefaultHasher::new();
    0xBBu8.hash(&mut h3);
    text.hash(&mut h3);
    let c = h3.finish();
    let mut h4 = std::collections::hash_map::DefaultHasher::new();
    0xCCu8.hash(&mut h4);
    text.hash(&mut h4);
    let d = h4.finish();
    out[0..8].copy_from_slice(&a.to_le_bytes());
    out[8..16].copy_from_slice(&b.to_le_bytes());
    out[16..24].copy_from_slice(&c.to_le_bytes());
    out[24..32].copy_from_slice(&d.to_le_bytes());
    out
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
