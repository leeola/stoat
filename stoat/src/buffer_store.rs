//! Buffer storage and management.
//!
//! Provides centralized buffer management with HashMap-based storage, following
//! Zed's BufferStore pattern for performance. Manages the lifecycle of buffers,
//! path-to-buffer mapping, and buffer activation history.

use crate::buffer_item::BufferItem;
use gpui::{App, AppContext, Entity};
use std::{collections::HashMap, num::NonZeroU64, path::PathBuf};
use stoat_text::Language;
use text::{Buffer, BufferId};

/// Open buffer state.
///
/// Wraps [`Entity<BufferItem>`] and associated metadata. Buffers are stored
/// by BufferId and can be looked up by path.
pub struct OpenBuffer {
    /// The buffer item entity
    pub buffer_item: Entity<BufferItem>,
    /// File path (None for scratch buffers)
    pub path: Option<PathBuf>,
}

/// Central buffer storage and management.
///
/// Manages all open buffers with efficient HashMap-based lookup by both BufferId
/// and file path. Maintains buffer activation history for MRU (most recently used)
/// ordering. Based on Zed's BufferStore architecture for performance parity.
///
/// # Usage
///
/// Created once at editor initialization and stored in [`Stoat`]:
///
/// ```rust,ignore
/// let buffer_store = cx.new(|_| BufferStore::new());
/// ```
///
/// Buffers are opened via [`open_buffer`] and accessed via [`get_buffer`]:
///
/// ```rust,ignore
/// let buffer_id = buffer_store.update(cx, |store, cx| {
///     store.open_buffer(Some(path), Language::Rust, cx)
/// })?;
/// ```
pub struct BufferStore {
    /// All open buffers indexed by BufferId
    buffers: HashMap<BufferId, OpenBuffer>,
    /// Path to BufferId mapping for quick lookup
    path_to_buffer: HashMap<PathBuf, BufferId>,
    /// Buffer activation history (most recent last)
    activation_history: Vec<BufferId>,
    /// Next buffer ID to allocate
    next_buffer_id: u64,
}

impl BufferStore {
    /// Create a new empty buffer store.
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            path_to_buffer: HashMap::new(),
            activation_history: Vec::new(),
            next_buffer_id: 1,
        }
    }

    /// Open or create a buffer.
    ///
    /// If a buffer for the given path already exists, returns its BufferId.
    /// Otherwise creates a new buffer and returns its BufferId.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (None for scratch buffers)
    /// * `language` - Language for syntax highlighting
    /// * `cx` - Context for creating entities
    ///
    /// # Returns
    ///
    /// BufferId of the opened or created buffer
    pub fn open_buffer(
        &mut self,
        path: Option<PathBuf>,
        language: Language,
        cx: &mut App,
    ) -> BufferId {
        // Check if buffer already exists for this path
        if let Some(path) = &path {
            if let Some(&buffer_id) = self.path_to_buffer.get(path) {
                // Update activation history
                self.activation_history.retain(|&id| id != buffer_id);
                self.activation_history.push(buffer_id);
                return buffer_id;
            }
        }

        // Create new buffer
        let buffer_id = self.allocate_buffer_id();
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, language, cx));

        // Store buffer
        let open_buffer = OpenBuffer {
            buffer_item,
            path: path.clone(),
        };
        self.buffers.insert(buffer_id, open_buffer);

        // Update path mapping if path exists
        if let Some(path) = path {
            self.path_to_buffer.insert(path, buffer_id);
        }

        // Update activation history
        self.activation_history.push(buffer_id);

        buffer_id
    }

    /// Get a buffer by ID.
    ///
    /// Returns [`None`] if the buffer doesn't exist.
    pub fn get_buffer(&self, buffer_id: BufferId) -> Option<&OpenBuffer> {
        self.buffers.get(&buffer_id)
    }

    /// Get a buffer by path.
    ///
    /// Returns [`None`] if no buffer exists for the path.
    pub fn get_buffer_by_path(&self, path: &PathBuf) -> Option<&OpenBuffer> {
        self.path_to_buffer
            .get(path)
            .and_then(|id| self.buffers.get(id))
    }

    /// Close a buffer.
    ///
    /// Removes the buffer from storage and activation history.
    /// Returns true if the buffer was found and closed.
    pub fn close_buffer(&mut self, buffer_id: BufferId) -> bool {
        if let Some(open_buffer) = self.buffers.remove(&buffer_id) {
            // Remove from path mapping
            if let Some(path) = open_buffer.path {
                self.path_to_buffer.remove(&path);
            }

            // Remove from activation history
            self.activation_history.retain(|&id| id != buffer_id);
            true
        } else {
            false
        }
    }

    /// Get all buffer IDs in activation order (most recent last).
    pub fn buffer_ids_by_activation(&self) -> &[BufferId] {
        &self.activation_history
    }

    /// Get all open buffer paths.
    pub fn buffer_paths(&self) -> Vec<PathBuf> {
        self.buffers
            .values()
            .filter_map(|b| b.path.clone())
            .collect()
    }

    /// Update activation history when switching to a buffer.
    ///
    /// Call this when the user switches to a buffer to update MRU ordering.
    pub fn activate_buffer(&mut self, buffer_id: BufferId) {
        if self.buffers.contains_key(&buffer_id) {
            self.activation_history.retain(|&id| id != buffer_id);
            self.activation_history.push(buffer_id);
        }
    }

    /// Allocate a new unique buffer ID.
    fn allocate_buffer_id(&mut self) -> BufferId {
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;
        BufferId::from(NonZeroU64::new(id).unwrap())
    }
}
