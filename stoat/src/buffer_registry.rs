use crate::buffer::{BufferId, SharedBuffer, TextBuffer};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use stoat_language::{Language, SyntaxState};

struct BufferEntry {
    buffer: SharedBuffer,
    path: Option<PathBuf>,
    language: Option<Arc<Language>>,
    syntax: Option<SyntaxState>,
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
            },
        );
        (id, buffer)
    }

    pub(crate) fn get(&self, id: BufferId) -> Option<SharedBuffer> {
        self.buffers.get(&id).map(|e| e.buffer.clone())
    }

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
        }
    }

    pub(crate) fn syntax_version(&self, id: BufferId) -> Option<u64> {
        self.buffers.get(&id)?.syntax.as_ref().map(|s| s.version)
    }

    pub(crate) fn store_syntax(&mut self, id: BufferId, state: SyntaxState) {
        if let Some(entry) = self.buffers.get_mut(&id) {
            entry.syntax = Some(state);
        }
    }
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
}
