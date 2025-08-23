//! Buffer management for text editing
//!
//! Provides centralized buffer management for the Stoat editor. Buffers are the core
//! data structure for text storage, replacing the previous node-based architecture.
//! The [`BufferManager`] maintains all buffers and provides operations for creating,
//! loading, saving, and switching between buffers.
//!
//! This module integrates with the [`stoat_text`] crate for the underlying text
//! editing capabilities while providing a higher-level interface for buffer
//! lifecycle management.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use stoat_text::buffer::Buffer;

/// Unique identifier for buffers
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferId(pub u64);

impl std::fmt::Display for BufferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buffer:{}", self.0)
    }
}

/// Information about a buffer that can be serialized
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferInfo {
    /// Buffer identifier
    pub id: BufferId,
    /// Display name for the buffer
    pub name: String,
    /// Optional file path if buffer is associated with a file
    pub file_path: Option<PathBuf>,
    /// Language/syntax for the buffer
    pub language: Option<String>,
    /// Whether the buffer has unsaved changes
    pub dirty: bool,
}

/// Serializable representation of buffer state for workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableBuffer {
    /// Buffer metadata
    pub info: BufferInfo,
    /// Text content of the buffer
    pub content: String,
    /// Cursor positions (for multiple cursors)
    pub cursors: Vec<(usize, usize)>, // (token_index, char_offset)
}

/// Central manager for all text buffers in the editor
///
/// The [`BufferManager`] provides a centralized interface for buffer operations,
/// replacing the previous node-based architecture. It maintains the buffer registry,
/// handles buffer lifecycle, and provides methods for creating, loading, and
/// managing text buffers.
#[derive(Debug)]
pub struct BufferManager {
    /// Map of buffer IDs to buffers
    buffers: HashMap<BufferId, Buffer>,
    /// Next available buffer ID
    next_id: u64,
    /// Currently active buffer
    active_buffer: Option<BufferId>,
    /// Buffer information cache
    buffer_info: HashMap<BufferId, BufferInfo>,
    /// Recently accessed buffers (most recent first)
    recent_buffers: Vec<BufferId>,
    /// Maximum number of recent buffers to track
    max_recent: usize,
}

impl Default for BufferManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferManager {
    /// Create a new buffer manager
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            next_id: 1,
            active_buffer: None,
            buffer_info: HashMap::new(),
            recent_buffers: Vec::new(),
            max_recent: 10,
        }
    }

    /// Create a new empty buffer
    pub fn create_buffer(&mut self, name: String) -> BufferId {
        let id = BufferId(self.next_id);
        self.next_id += 1;

        // Create an empty rope AST
        let rope = self.create_rope_from_content("");
        let buffer = Buffer::from_rope(rope, id.0);

        let info = BufferInfo {
            id,
            name,
            file_path: None,
            language: None,
            dirty: false,
        };

        self.buffers.insert(id, buffer);
        self.buffer_info.insert(id, info);
        self.add_to_recent(id);

        // Set as active (newly created buffers become active)
        self.active_buffer = Some(id);

        id
    }

    /// Create a buffer from file content
    pub fn create_buffer_from_file(&mut self, path: PathBuf) -> Result<BufferId> {
        let content = std::fs::read_to_string(&path).map_err(|e| Error::Io {
            message: format!("Failed to read file {}: {}", path.display(), e),
        })?;

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("untitled")
            .to_string();

        let id = self.create_buffer_with_content(name, content);

        // Update buffer info with file path
        let language = self.infer_language_from_path(&Some(path.clone()));
        if let Some(info) = self.buffer_info.get_mut(&id) {
            info.file_path = Some(path);
            info.language = language;
        }

        Ok(id)
    }

    /// Create a buffer with initial content
    pub fn create_buffer_with_content(&mut self, name: String, content: String) -> BufferId {
        let id = BufferId(self.next_id);
        self.next_id += 1;

        // Create rope AST from content
        let rope = self.create_rope_from_content(&content);
        let buffer = Buffer::from_rope(rope, id.0);

        let info = BufferInfo {
            id,
            name,
            file_path: None,
            language: None,
            dirty: false,
        };

        self.buffers.insert(id, buffer);
        self.buffer_info.insert(id, info);
        self.add_to_recent(id);

        // Set as active (newly created buffers become active)
        self.active_buffer = Some(id);

        id
    }

    /// Create a scratch buffer (temporary, not associated with a file)
    pub fn create_scratch_buffer(&mut self, name: String) -> BufferId {
        self.create_buffer(format!("*{}*", name))
    }

    /// Get a buffer by ID
    pub fn get(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    /// Get a mutable buffer by ID
    pub fn get_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        if self.buffers.contains_key(&id) {
            self.add_to_recent(id);
            // Mark buffer as dirty when accessed mutably
            if let Some(info) = self.buffer_info.get_mut(&id) {
                info.dirty = true;
            }
            self.buffers.get_mut(&id)
        } else {
            None
        }
    }

    /// Get buffer information
    pub fn get_info(&self, id: BufferId) -> Option<&BufferInfo> {
        self.buffer_info.get(&id)
    }

    /// List all buffers
    pub fn list_buffers(&self) -> Vec<(BufferId, &BufferInfo)> {
        self.buffer_info
            .iter()
            .map(|(id, info)| (*id, info))
            .collect()
    }

    /// Find buffer by file path
    pub fn find_buffer_by_path(&self, path: &std::path::Path) -> Option<BufferId> {
        self.buffer_info
            .iter()
            .find(|(_, info)| {
                info.file_path
                    .as_ref()
                    .map_or(false, |buf_path| buf_path == path)
            })
            .map(|(id, _)| *id)
    }

    /// Kill a buffer (remove it from the manager)
    pub fn kill_buffer(&mut self, id: BufferId) -> Result<()> {
        if !self.buffers.contains_key(&id) {
            return Err(Error::Generic {
                message: format!("Buffer {} not found", id),
            });
        }

        // Check if buffer has unsaved changes
        if let Some(info) = self.buffer_info.get(&id) {
            if info.dirty {
                return Err(Error::Generic {
                    message: format!("Buffer {} has unsaved changes", info.name),
                });
            }
        }

        self.buffers.remove(&id);
        self.buffer_info.remove(&id);
        self.recent_buffers.retain(|&buf_id| buf_id != id);

        // If this was the active buffer, switch to another
        if self.active_buffer == Some(id) {
            self.active_buffer = self.recent_buffers.first().copied();
        }

        Ok(())
    }

    /// Save a buffer to its associated file
    pub fn save_buffer(&mut self, id: BufferId) -> Result<()> {
        let buffer = self.buffers.get(&id).ok_or_else(|| Error::Generic {
            message: format!("Buffer {} not found", id),
        })?;

        let info = self
            .buffer_info
            .get_mut(&id)
            .ok_or_else(|| Error::Generic {
                message: format!("Buffer info {} not found", id),
            })?;

        let path = info.file_path.as_ref().ok_or_else(|| Error::Generic {
            message: format!("Buffer {} is not associated with a file", info.name),
        })?;

        let content = buffer.rope().to_string();
        std::fs::write(path, content).map_err(|e| Error::Io {
            message: format!("Failed to write file {}: {}", path.display(), e),
        })?;

        // Mark buffer as clean
        info.dirty = false;

        Ok(())
    }

    /// Rename a buffer
    pub fn rename_buffer(&mut self, id: BufferId, new_name: String) -> Result<()> {
        let info = self
            .buffer_info
            .get_mut(&id)
            .ok_or_else(|| Error::Generic {
                message: format!("Buffer {} not found", id),
            })?;

        info.name = new_name;
        Ok(())
    }

    /// Switch to a buffer (makes it active)
    pub fn switch_to_buffer(&mut self, id: BufferId) -> Result<()> {
        if !self.buffers.contains_key(&id) {
            return Err(Error::Generic {
                message: format!("Buffer {} not found", id),
            });
        }

        self.active_buffer = Some(id);
        self.add_to_recent(id);
        Ok(())
    }

    /// Get the currently active buffer
    pub fn active_buffer(&self) -> Option<BufferId> {
        self.active_buffer
    }

    /// Switch to the next buffer in the recent list
    pub fn next_buffer(&mut self) {
        if let Some(current) = self.active_buffer {
            if let Some(pos) = self.recent_buffers.iter().position(|&id| id == current) {
                let next_pos = (pos + 1) % self.recent_buffers.len();
                if let Some(&next_id) = self.recent_buffers.get(next_pos) {
                    self.active_buffer = Some(next_id);
                }
            }
        } else if let Some(&first) = self.recent_buffers.first() {
            self.active_buffer = Some(first);
        }
    }

    /// Switch to the previous buffer in the recent list
    pub fn previous_buffer(&mut self) {
        if let Some(current) = self.active_buffer {
            if let Some(pos) = self.recent_buffers.iter().position(|&id| id == current) {
                let prev_pos = if pos == 0 {
                    self.recent_buffers.len() - 1
                } else {
                    pos - 1
                };
                if let Some(&prev_id) = self.recent_buffers.get(prev_pos) {
                    self.active_buffer = Some(prev_id);
                }
            }
        } else if let Some(&first) = self.recent_buffers.first() {
            self.active_buffer = Some(first);
        }
    }

    /// Get the list of recently accessed buffers
    pub fn recent_buffers(&self) -> Vec<BufferId> {
        self.recent_buffers.clone()
    }

    /// Get all buffers for serialization
    pub fn get_serializable_buffers(&self) -> Vec<SerializableBuffer> {
        self.buffers
            .iter()
            .filter_map(|(id, buffer)| {
                self.buffer_info.get(id).map(|info| SerializableBuffer {
                    info: info.clone(),
                    content: buffer.rope().to_string(),
                    cursors: vec![(0, 0)], // TODO: Get actual cursor positions
                })
            })
            .collect()
    }

    /// Restore buffers from serialized data
    pub fn restore_from_serializable(&mut self, buffers: Vec<SerializableBuffer>) {
        for serializable in buffers {
            let id = serializable.info.id;

            // Update next_id to ensure no conflicts
            if id.0 >= self.next_id {
                self.next_id = id.0 + 1;
            }

            // Create rope from content
            let rope = self.create_rope_from_content(&serializable.content);
            let buffer = Buffer::from_rope(rope, id.0);

            self.buffers.insert(id, buffer);
            self.buffer_info.insert(id, serializable.info);
            self.add_to_recent(id);

            // Set as active if we don't have one
            if self.active_buffer.is_none() {
                self.active_buffer = Some(id);
            }
        }
    }

    /// Add buffer to recent list
    fn add_to_recent(&mut self, id: BufferId) {
        // Remove if already in list
        self.recent_buffers.retain(|&buf_id| buf_id != id);

        // Add to front
        self.recent_buffers.insert(0, id);

        // Trim to max size
        if self.recent_buffers.len() > self.max_recent {
            self.recent_buffers.truncate(self.max_recent);
        }
    }

    /// Create a rope AST from text content
    fn create_rope_from_content(&self, content: &str) -> stoat_rope::RopeAst {
        use stoat_rope::{ast::TextRange, builder::AstBuilder, kind::SyntaxKind};

        if content.is_empty() {
            // Create minimal empty document structure
            let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 0)).finish();
            return stoat_rope::RopeAst::from_root(doc);
        }

        // Split content into lines and create tokens
        let lines: Vec<&str> = content.lines().collect();
        let mut tokens = Vec::new();
        let mut offset = 0;

        for (i, line) in lines.iter().enumerate() {
            if !line.is_empty() {
                let start = offset;
                let end = offset + line.len();
                tokens.push(AstBuilder::token(
                    SyntaxKind::Text,
                    *line,
                    TextRange::new(start, end),
                ));
                offset = end;
            }

            // Add newline token except for the last line
            if i < lines.len() - 1 {
                let start = offset;
                let end = offset + 1;
                tokens.push(AstBuilder::token(
                    SyntaxKind::Newline,
                    "\n",
                    TextRange::new(start, end),
                ));
                offset = end;
            }
        }

        // Create paragraph containing all tokens
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, TextRange::new(0, offset))
            .add_children(tokens)
            .finish();

        // Create document containing the paragraph
        let doc = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, offset))
            .add_child(paragraph)
            .finish();

        stoat_rope::RopeAst::from_root(doc)
    }

    /// Infer language from file path
    fn infer_language_from_path(&self, path: &Option<PathBuf>) -> Option<String> {
        path.as_ref()
            .and_then(|p| p.extension())
            .and_then(|ext| ext.to_str())
            .map(|ext| match ext.to_lowercase().as_str() {
                "rs" => "rust",
                "py" => "python",
                "js" => "javascript",
                "ts" => "typescript",
                "md" => "markdown",
                "txt" => "text",
                "json" => "json",
                "toml" => "toml",
                "yaml" | "yml" => "yaml",
                _ => "text",
            })
            .map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_buffer_manager_creation() {
        let manager = BufferManager::new();
        assert_eq!(manager.list_buffers().len(), 0);
        assert!(manager.active_buffer().is_none());
    }

    #[test]
    fn test_create_buffer() {
        let mut manager = BufferManager::new();
        let id = manager.create_buffer("test".to_string());

        assert_eq!(manager.list_buffers().len(), 1);
        assert_eq!(manager.active_buffer(), Some(id));

        let buffer = manager.get(id).expect("Buffer should exist");
        assert_eq!(buffer.id(), id.0);
    }

    #[test]
    fn test_buffer_with_content() {
        let mut manager = BufferManager::new();
        let content = "Hello, world!\nThis is a test.";
        let id = manager.create_buffer_with_content("test".to_string(), content.to_string());

        let buffer = manager.get(id).expect("Buffer should exist");
        assert_eq!(buffer.rope().to_string(), content);
    }

    #[test]
    fn test_buffer_file_operations() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("test.txt");
        let content = "Test file content";

        std::fs::write(&file_path, content).expect("Failed to write test file");

        let mut manager = BufferManager::new();
        let id = manager
            .create_buffer_from_file(file_path.clone())
            .expect("Failed to create buffer from file");

        let buffer = manager.get(id).expect("Buffer should exist");
        assert_eq!(buffer.rope().to_string(), content);

        let info = manager.get_info(id).expect("Buffer info should exist");
        assert_eq!(info.file_path, Some(file_path));
        assert_eq!(info.language, Some("text".to_string()));
    }

    #[test]
    fn test_buffer_switching() {
        let mut manager = BufferManager::new();
        let id1 = manager.create_buffer("buffer1".to_string());
        let _id2 = manager.create_buffer("buffer2".to_string());
        let id3 = manager.create_buffer("buffer3".to_string());

        // The last created buffer should be active
        assert_eq!(manager.active_buffer(), Some(id3));

        manager
            .switch_to_buffer(id1)
            .expect("Should switch to buffer1");
        assert_eq!(manager.active_buffer(), Some(id1));

        // Debug: print recent buffers to understand order
        let recent = manager.recent_buffers();
        println!("Recent buffers after switch: {:?}", recent);
        println!("Current active: {:?}", manager.active_buffer());

        manager.next_buffer();
        println!("After next_buffer: {:?}", manager.active_buffer());

        // The recent list should be [id1, id3, id2] after switching to id1
        // next_buffer from id1 (position 0) should go to position 1 which is id3
        assert_eq!(manager.active_buffer(), Some(id3));

        manager.previous_buffer();
        assert_eq!(manager.active_buffer(), Some(id1));
    }

    #[test]
    fn test_recent_buffers() {
        let mut manager = BufferManager::new();
        let id1 = manager.create_buffer("buffer1".to_string());
        let id2 = manager.create_buffer("buffer2".to_string());
        let id3 = manager.create_buffer("buffer3".to_string());

        let recent = manager.recent_buffers();
        assert_eq!(recent, vec![id3, id2, id1]);

        manager
            .switch_to_buffer(id1)
            .expect("Should switch to buffer1");
        let recent = manager.recent_buffers();
        assert_eq!(recent, vec![id1, id3, id2]);
    }

    #[test]
    fn test_buffer_serialization() {
        let mut manager = BufferManager::new();
        let id1 = manager.create_buffer_with_content("test1".to_string(), "Content 1".to_string());
        let id2 = manager.create_buffer_with_content("test2".to_string(), "Content 2".to_string());

        let serializable = manager.get_serializable_buffers();
        assert_eq!(serializable.len(), 2);

        let mut new_manager = BufferManager::new();
        new_manager.restore_from_serializable(serializable);

        assert_eq!(new_manager.list_buffers().len(), 2);
        assert!(new_manager.get(id1).is_some());
        assert!(new_manager.get(id2).is_some());
    }

    #[test]
    fn test_language_inference() {
        let manager = BufferManager::new();

        assert_eq!(
            manager.infer_language_from_path(&Some(PathBuf::from("test.rs"))),
            Some("rust".to_string())
        );
        assert_eq!(
            manager.infer_language_from_path(&Some(PathBuf::from("script.py"))),
            Some("python".to_string())
        );
        assert_eq!(
            manager.infer_language_from_path(&Some(PathBuf::from("unknown.xyz"))),
            Some("text".to_string())
        );
    }
}
