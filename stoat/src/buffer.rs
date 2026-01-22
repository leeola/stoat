use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use stoat_text::Rope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(u64);

pub struct TextBuffer {
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub dirty: bool,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            dirty: false,
        }
    }

    pub fn from_file(path: PathBuf) -> io::Result<Self> {
        let content = fs::read_to_string(&path)?;
        let mut rope = Rope::new();
        rope.push(&content);
        Ok(Self {
            rope,
            path: Some(path),
            dirty: false,
        })
    }

    pub fn line_count(&self) -> u32 {
        self.rope.summary().lines.row + 1
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedBuffer = Arc<RwLock<TextBuffer>>;

pub struct BufferStore {
    buffers: HashMap<BufferId, SharedBuffer>,
    next_id: u64,
    path_to_id: HashMap<PathBuf, BufferId>,
}

impl BufferStore {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            next_id: 0,
            path_to_id: HashMap::new(),
        }
    }

    pub fn open(&mut self, path: PathBuf) -> io::Result<(BufferId, SharedBuffer)> {
        let canonical = path.canonicalize()?;
        if let Some(&id) = self.path_to_id.get(&canonical) {
            let buffer = self.buffers.get(&id).expect("buffer id exists").clone();
            return Ok((id, buffer));
        }

        let text_buffer = TextBuffer::from_file(canonical.clone())?;
        let id = BufferId(self.next_id);
        self.next_id += 1;
        let shared = Arc::new(RwLock::new(text_buffer));
        self.buffers.insert(id, shared.clone());
        self.path_to_id.insert(canonical, id);
        Ok((id, shared))
    }

    pub fn create_scratch(&mut self) -> (BufferId, SharedBuffer) {
        let id = BufferId(self.next_id);
        self.next_id += 1;
        let shared = Arc::new(RwLock::new(TextBuffer::new()));
        self.buffers.insert(id, shared.clone());
        (id, shared)
    }
}

impl Default for BufferStore {
    fn default() -> Self {
        Self::new()
    }
}
