//! Buffer storage and management.
//!
//! Provides centralized buffer management with HashMap-based storage, following
//! Zed's BufferStore pattern for performance. Manages the lifecycle of buffers,
//! path-to-buffer mapping, and buffer activation history.

use crate::buffer::item::BufferItem;
use gpui::{App, AppContext, Entity, WeakEntity};
use std::{collections::HashMap, num::NonZeroU64, path::PathBuf};
use stoat_text::Language;
use text::{Buffer, BufferId};

/// Buffer list entry for UI display.
///
/// Represents a buffer in the buffer finder list, providing both display information
/// and lookup keys. Used by [`BufferStore::buffer_list`] to return all buffers
/// including unnamed/scratch buffers.
#[derive(Clone, Debug)]
pub struct BufferListEntry {
    /// Display name for UI (file path or "[Untitled]" for unnamed buffers)
    pub display_name: String,
    /// Buffer ID for lookup in [`BufferStore`]
    pub buffer_id: BufferId,
    /// Optional file path (None for unnamed/scratch buffers)
    pub path: Option<PathBuf>,
    /// Whether this is the active buffer in the current pane
    pub is_active: bool,
    /// Whether this buffer is visible in any pane
    pub is_visible: bool,
    /// Whether this buffer has unsaved modifications
    pub is_modified: bool,
}

/// Open buffer state.
///
/// Uses [`WeakEntity<BufferItem>`] following Zed's pattern to avoid memory leaks.
/// Strong references are held by [`Stoat::open_buffers`], while BufferStore tracks
/// buffers weakly. This allows automatic cleanup when all strong references are dropped.
pub struct OpenBuffer {
    /// Weak reference to buffer item (prevents memory leaks)
    pub buffer_item: WeakEntity<BufferItem>,
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

impl Default for BufferStore {
    fn default() -> Self {
        Self::new()
    }
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
    /// If a buffer for the given path already exists, attempts to upgrade its weak reference.
    /// Otherwise creates a new buffer. Returns both the BufferId and the strong Entity reference
    /// that the caller must store to keep the buffer alive.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (None for scratch buffers)
    /// * `language` - Language for syntax highlighting
    /// * `cx` - Context for creating entities
    ///
    /// # Returns
    ///
    /// `Option<(BufferId, Entity<BufferItem>)>` - BufferId and strong reference. Returns `None`
    /// if buffer existed but weak reference couldn't be upgraded (buffer was dropped).
    pub fn open_buffer(
        &mut self,
        path: Option<PathBuf>,
        language: Language,
        cx: &mut App,
    ) -> Option<(BufferId, Entity<BufferItem>)> {
        // Check if buffer already exists for this path
        if let Some(path) = &path {
            if let Some(&buffer_id) = self.path_to_buffer.get(path) {
                // Try to upgrade existing buffer
                if let Some(buffer_item) = self.get_buffer(buffer_id) {
                    // Update activation history
                    self.activation_history.retain(|&id| id != buffer_id);
                    self.activation_history.push(buffer_id);
                    return Some((buffer_id, buffer_item));
                } else {
                    // Weak reference is dead, clean it up
                    self.buffers.remove(&buffer_id);
                    self.path_to_buffer.remove(path);
                }
            }
        }

        // Create new buffer
        let buffer_id = self.allocate_buffer_id();
        tracing::trace!("open_buffer: buffer_id={:?}", buffer_id);
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, language, cx));

        // Store buffer with weak reference (strong ref must be held by caller)
        let open_buffer = OpenBuffer {
            buffer_item: buffer_item.downgrade(),
            path: path.clone(),
        };
        self.buffers.insert(buffer_id, open_buffer);

        // Update path mapping if path exists
        if let Some(path) = path {
            self.path_to_buffer.insert(path, buffer_id);
        }

        // Update activation history
        self.activation_history.push(buffer_id);

        Some((buffer_id, buffer_item))
    }

    /// Register an existing buffer item.
    ///
    /// Stores a weak reference to an already-created buffer. Useful when buffers are created
    /// outside BufferStore (e.g., initial welcome buffer).
    ///
    /// # Arguments
    ///
    /// * `buffer_id` - BufferId of the buffer
    /// * `buffer_item` - The buffer item to register
    /// * `path` - Optional file path
    pub fn register_buffer(
        &mut self,
        buffer_id: BufferId,
        buffer_item: &Entity<BufferItem>,
        path: Option<PathBuf>,
    ) {
        let open_buffer = OpenBuffer {
            buffer_item: buffer_item.downgrade(),
            path: path.clone(),
        };
        self.buffers.insert(buffer_id, open_buffer);

        if let Some(path) = path {
            self.path_to_buffer.insert(path, buffer_id);
        }

        self.activation_history.push(buffer_id);
    }

    /// Get a buffer by ID.
    ///
    /// Attempts to upgrade the weak reference. Returns [`None`] if the buffer doesn't exist
    /// or if the weak reference couldn't be upgraded (buffer was dropped).
    pub fn get_buffer(&self, buffer_id: BufferId) -> Option<Entity<BufferItem>> {
        let result = self.buffers.get(&buffer_id)?.buffer_item.upgrade();
        if result.is_none() {
            tracing::trace!("Failed to upgrade weak ref for buffer_id: {:?}", buffer_id);
        }
        result
    }

    /// Get a buffer by path.
    ///
    /// Attempts to upgrade the weak reference. Returns [`None`] if no buffer exists for the path
    /// or if the weak reference couldn't be upgraded (buffer was dropped).
    pub fn get_buffer_by_path(&self, path: &PathBuf) -> Option<Entity<BufferItem>> {
        let buffer_id = self.path_to_buffer.get(path)?;
        self.get_buffer(*buffer_id)
    }

    /// Get the file path for a buffer ID.
    ///
    /// Returns the file path associated with a buffer, or [`None`] if the buffer doesn't exist
    /// or has no associated file path (unnamed/scratch buffer).
    ///
    /// # Arguments
    ///
    /// * `buffer_id` - The buffer ID to look up
    ///
    /// # Returns
    ///
    /// Reference to the file path, or [`None`] if buffer has no path
    ///
    /// # Usage
    ///
    /// Used by [`crate::Stoat::write_all`] to get paths for all open buffers when saving.
    pub fn get_path(&self, buffer_id: BufferId) -> Option<&PathBuf> {
        self.buffers.get(&buffer_id)?.path.as_ref()
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

    /// Get all open buffers as list entries for UI display.
    ///
    /// Returns all buffers including unnamed/scratch buffers. Unnamed buffers
    /// get display name "[Untitled]". Only includes buffers whose weak references
    /// can be upgraded (alive buffers).
    ///
    /// # Arguments
    ///
    /// * `active_id` - The currently active buffer ID (in the current pane)
    /// * `visible_ids` - Buffer IDs visible in any pane
    /// * `cx` - App context for reading buffer modification state
    ///
    /// Used by buffer finder to show all open buffers with status flags.
    pub fn buffer_list(
        &self,
        active_id: Option<BufferId>,
        visible_ids: &[BufferId],
        cx: &gpui::App,
    ) -> Vec<BufferListEntry> {
        self.buffers
            .iter()
            .filter_map(|(buffer_id, open_buffer)| {
                // Only include buffers that are still alive (can upgrade weak ref)
                let buffer_item = open_buffer.buffer_item.upgrade()?;

                let (display_name, path) = if let Some(path) = &open_buffer.path {
                    (path.display().to_string(), Some(path.clone()))
                } else {
                    ("[Untitled]".to_string(), None)
                };

                let is_active = active_id == Some(*buffer_id);
                let is_visible = visible_ids.contains(buffer_id);
                let is_modified = buffer_item.read(cx).is_modified(cx);

                Some(BufferListEntry {
                    display_name,
                    buffer_id: *buffer_id,
                    path,
                    is_active,
                    is_visible,
                    is_modified,
                })
            })
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
    ///
    /// This ensures no BufferID collisions by incrementing an internal counter.
    /// Public so that external code (like Stoat::new) can allocate IDs before
    /// creating buffers to register them properly.
    pub fn allocate_buffer_id(&mut self) -> BufferId {
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;
        BufferId::from(NonZeroU64::new(id).unwrap())
    }
}
