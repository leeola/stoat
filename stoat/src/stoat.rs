//! Core Stoat editor entity with Context<Self> pattern.
//!
//! This follows Zed's Buffer architecture - Stoat is an Entity that can spawn
//! self-updating async tasks.

use crate::{
    buffer_item::BufferItem,
    buffer_store::BufferStore,
    cursor::CursorManager,
    file_finder::PreviewData,
    git_diff::BufferDiff,
    git_repository::Repository,
    scroll::ScrollPosition,
    worktree::{Entry, Worktree},
};
use gpui::{App, AppContext, Context, Entity, EventEmitter, Task};
use nucleo_matcher::{Config, Matcher};
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use stoat_text::Language;
use text::{Buffer, BufferId, Point};

/// Mode metadata for editor modes.
///
/// Contains display information for a mode. Modes define different editor behaviors
/// (normal, insert, visual, etc.) and have associated keybindings and display names.
#[derive(Clone, Debug)]
pub struct Mode {
    /// Internal identifier for the mode (e.g., "normal", "insert")
    pub name: String,
    /// Display name shown to users (e.g., "NORMAL", "INSERT")
    pub display_name: String,
    /// Override mode to return to when this mode is dismissed.
    ///
    /// If set, this mode will always return to the specified mode when dismissed,
    /// ignoring the actual previous mode. Used for overlay modes like file_finder
    /// and command_palette that should always return to normal.
    pub previous: Option<String>,
}

impl Mode {
    /// Create a new mode with the given name and display name.
    pub fn new(name: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            previous: None,
        }
    }

    /// Create a new mode with an explicit previous mode override.
    pub fn with_previous(
        name: impl Into<String>,
        display_name: impl Into<String>,
        previous: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            previous: Some(previous.into()),
        }
    }
}

/// Command information for command palette.
///
/// Contains metadata about an action that can be executed through the command palette.
/// Used for fuzzy searching and dynamic dispatch via [`std::any::TypeId`].
#[derive(Clone, Debug)]
pub struct CommandInfo {
    /// Action name (e.g., "MoveLeft", "Save")
    pub name: String,
    /// Description of what the command does
    pub description: String,
    /// TypeId for dispatching the action
    pub type_id: std::any::TypeId,
}

/// Events emitted by Stoat
#[derive(Clone, Debug)]
pub enum StoatEvent {
    /// Editor content or state changed
    Changed,
}

/// Main editor entity.
///
/// Key difference from old stoat: methods take `&mut Context<Self>` instead of `&mut App`.
/// This enables spawning self-updating async tasks.
pub struct Stoat {
    /// Buffer storage and management (tracks with WeakEntity)
    pub(crate) buffer_store: Entity<BufferStore>,

    /// Open buffer items (holds strong references following Zed's pattern)
    ///
    /// This vec holds strong `Entity<BufferItem>` references to keep buffers alive.
    /// BufferStore tracks buffers weakly, so without these strong refs, buffers would
    /// be immediately dropped. This matches Zed's Pane architecture at a simpler scale.
    pub(crate) open_buffers: Vec<Entity<BufferItem>>,

    /// Currently active buffer ID
    pub(crate) active_buffer_id: Option<BufferId>,

    /// Cursor position management
    pub(crate) cursor: CursorManager,

    /// Scroll position with animation
    pub(crate) scroll: ScrollPosition,

    /// Viewport height in lines
    pub(crate) viewport_lines: Option<f32>,

    /// Current mode (normal, insert, file_finder)
    pub(crate) mode: String,

    /// Registry of available modes
    pub(crate) modes: HashMap<String, Mode>,

    // File finder state
    pub(crate) file_finder_input: Option<Entity<Buffer>>,
    pub(crate) file_finder_files: Vec<Entry>,
    pub(crate) file_finder_filtered: Vec<PathBuf>,
    pub(crate) file_finder_selected: usize,
    pub(crate) file_finder_previous_mode: Option<String>,
    pub(crate) file_finder_preview: Option<PreviewData>,
    pub(crate) file_finder_preview_task: Option<Task<()>>,
    pub(crate) file_finder_matcher: Matcher,

    // Command palette state
    pub(crate) command_palette_input: Option<Entity<Buffer>>,
    pub(crate) command_palette_commands: Vec<CommandInfo>,
    pub(crate) command_palette_filtered: Vec<CommandInfo>,
    pub(crate) command_palette_selected: usize,
    pub(crate) command_palette_previous_mode: Option<String>,

    // Buffer finder state
    pub(crate) buffer_finder_input: Option<Entity<Buffer>>,
    pub(crate) buffer_finder_buffers: Vec<PathBuf>,
    pub(crate) buffer_finder_filtered: Vec<PathBuf>,
    pub(crate) buffer_finder_selected: usize,
    pub(crate) buffer_finder_previous_mode: Option<String>,

    // Git status state
    pub(crate) git_status_files: Vec<crate::git_status::GitStatusEntry>,
    pub(crate) git_status_selected: usize,
    pub(crate) git_status_previous_mode: Option<String>,
    pub(crate) git_status_preview: Option<crate::git_status::DiffPreviewData>,
    pub(crate) git_status_preview_task: Option<Task<()>>,
    pub(crate) git_status_branch_info: Option<crate::git_status::GitBranchInfo>,
    pub(crate) git_dirty_count: usize,

    /// Current file path (for status bar display)
    pub(crate) current_file_path: Option<PathBuf>,

    /// Worktree for file scanning
    pub(crate) worktree: Arc<Mutex<Worktree>>,
}

impl EventEmitter<StoatEvent> for Stoat {}

impl Stoat {
    /// Create new Stoat entity.
    ///
    /// Takes `&mut Context<Self>` to follow Zed's Buffer pattern.
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Create buffer store
        let buffer_store = cx.new(|_| BufferStore::new());

        // Allocate buffer ID from BufferStore to prevent collisions
        let buffer_id = buffer_store.update(cx, |store, _cx| store.allocate_buffer_id());

        // Create initial welcome buffer
        let welcome_text = "Welcome to Stoat v4!\n\nPress 'i' to enter insert mode.\nType some text.\nPress Esc to return to normal mode.\n\nPress 'h', 'j', 'k', 'l' to move in normal mode.";
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, welcome_text));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, Language::PlainText, cx));

        // Register buffer in BufferStore (weak ref) and store strong ref in open_buffers
        buffer_store.update(cx, |store, _cx| {
            store.register_buffer(buffer_id, &buffer_item, None);
        });
        let open_buffers = vec![buffer_item.clone()];
        let active_buffer_id = Some(buffer_id);

        let worktree = Arc::new(Mutex::new(Worktree::new(PathBuf::from("."))));

        // Initialize mode registry from keymap.toml
        let modes = crate::keymap::parse_modes_from_config();

        // Initialize git status for status bar
        let (git_branch_info, git_status_files, git_dirty_count) =
            if let Ok(repo) = Repository::open(std::path::Path::new(".")) {
                let branch_info = crate::git_status::gather_git_branch_info(repo.inner());
                let status_files = crate::git_status::gather_git_status(repo.inner())
                    .unwrap_or_else(|_| Vec::new());
                let dirty_count = status_files.len();
                (branch_info, status_files, dirty_count)
            } else {
                (None, Vec::new(), 0)
            };

        Self {
            buffer_store,
            open_buffers,
            active_buffer_id,
            cursor: CursorManager::new(),
            scroll: ScrollPosition::new(),
            viewport_lines: None,
            mode: "normal".into(),
            modes,
            file_finder_input: None,
            file_finder_files: Vec::new(),
            file_finder_filtered: Vec::new(),
            file_finder_selected: 0,
            file_finder_previous_mode: None,
            file_finder_preview: None,
            file_finder_preview_task: None,
            file_finder_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            command_palette_input: None,
            command_palette_commands: Vec::new(),
            command_palette_filtered: Vec::new(),
            command_palette_selected: 0,
            command_palette_previous_mode: None,
            buffer_finder_input: None,
            buffer_finder_buffers: Vec::new(),
            buffer_finder_filtered: Vec::new(),
            buffer_finder_selected: 0,
            buffer_finder_previous_mode: None,
            git_status_files,
            git_status_selected: 0,
            git_status_previous_mode: None,
            git_status_preview: None,
            git_status_preview_task: None,
            git_status_branch_info: git_branch_info,
            git_dirty_count,
            current_file_path: None,
            worktree,
        }
    }

    /// Clone this Stoat for a split pane.
    ///
    /// Creates a new Stoat instance that shares the same [`BufferItem`] (text buffer)
    /// but has independent cursor position, scroll position, and viewport state.
    /// This follows Zed's performant pattern: share expensive buffer data via [`Entity`],
    /// but maintain independent view state per pane.
    ///
    /// The cursor and scroll positions are copied to the new instance, so the split starts
    /// at the same view, but subsequent movements are independent.
    ///
    /// Used by [`PaneGroupView`] when splitting panes to create multiple views of the same buffer.
    pub fn clone_for_split(&self) -> Self {
        Self {
            buffer_store: self.buffer_store.clone(),
            open_buffers: self.open_buffers.clone(),
            active_buffer_id: self.active_buffer_id,
            cursor: self.cursor.clone(),
            scroll: self.scroll.clone(),
            viewport_lines: self.viewport_lines,
            mode: self.mode.clone(),
            modes: self.modes.clone(),
            file_finder_input: None,
            file_finder_files: Vec::new(),
            file_finder_filtered: Vec::new(),
            file_finder_selected: 0,
            file_finder_previous_mode: None,
            file_finder_preview: None,
            file_finder_preview_task: None,
            file_finder_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            command_palette_input: None,
            command_palette_commands: Vec::new(),
            command_palette_filtered: Vec::new(),
            command_palette_selected: 0,
            command_palette_previous_mode: None,
            buffer_finder_input: None,
            buffer_finder_buffers: Vec::new(),
            buffer_finder_filtered: Vec::new(),
            buffer_finder_selected: 0,
            buffer_finder_previous_mode: None,
            git_status_files: self.git_status_files.clone(),
            git_status_selected: 0,
            git_status_previous_mode: None,
            git_status_preview: None,
            git_status_preview_task: None,
            git_status_branch_info: self.git_status_branch_info.clone(),
            git_dirty_count: self.git_dirty_count,
            current_file_path: self.current_file_path.clone(),
            worktree: self.worktree.clone(),
        }
    }

    /// Get the currently active buffer item.
    ///
    /// Looks up the active buffer by `active_buffer_id` in BufferStore and upgrades
    /// the weak reference. Returns `None` if no buffer is active or if the weak
    /// reference couldn't be upgraded (buffer was dropped).
    ///
    /// This is the new way to access buffers - it will replace direct `buffer_item` access.
    pub fn active_buffer_item(&self, cx: &App) -> Option<Entity<BufferItem>> {
        let buffer_id = self.active_buffer_id?;
        self.buffer_store.read(cx).get_buffer(buffer_id)
    }

    /// Get the currently active buffer (convenience wrapper).
    ///
    /// This is a convenience method that unwraps the result from [`active_buffer_item`].
    /// Panics if no buffer is active (should never happen in practice).
    pub fn active_buffer(&self, cx: &App) -> Entity<BufferItem> {
        let buffer_id = match self.active_buffer_id {
            Some(id) => id,
            None => {
                tracing::error!(
                    "active_buffer called but active_buffer_id is None! open_buffers.len={}",
                    self.open_buffers.len()
                );
                panic!("No active buffer - active_buffer_id is None");
            },
        };

        match self.buffer_store.read(cx).get_buffer(buffer_id) {
            Some(item) => item,
            None => {
                tracing::error!(
                    "Failed to get buffer for id {:?}!\n\
                     active_buffer_id: {:?}\n\
                     open_buffers.len: {}\n\
                     open_buffers buffer_ids: {:?}",
                    buffer_id,
                    self.active_buffer_id,
                    self.open_buffers.len(),
                    self.open_buffers
                        .iter()
                        .map(|b| b.read(cx).buffer().read(cx).remote_id())
                        .collect::<Vec<_>>()
                );
                panic!(
                    "No active buffer - weak reference upgrade failed for buffer_id {:?}",
                    buffer_id
                );
            },
        }
    }

    /// Get cursor position
    pub fn cursor_position(&self) -> Point {
        self.cursor.position()
    }

    /// Set cursor position
    pub fn set_cursor_position(&mut self, position: Point) {
        self.cursor.move_to(position);
    }

    /// Get current selection
    pub fn selection(&self) -> &crate::cursor::Selection {
        self.cursor.selection()
    }

    /// Get scroll position
    pub fn scroll_position(&self) -> gpui::Point<f32> {
        self.scroll.position
    }

    /// Get current mode
    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// Set mode
    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
    }

    /// Get mode metadata by name.
    ///
    /// Returns the [`Mode`] struct containing display name and other metadata
    /// for the given mode name, or `None` if the mode is not registered.
    pub fn get_mode(&self, name: &str) -> Option<&Mode> {
        self.modes.get(name)
    }

    /// Set viewport height in lines
    pub fn set_viewport_lines(&mut self, lines: f32) {
        self.viewport_lines = Some(lines);
    }

    /// Update scroll animation
    pub fn update_scroll_animation(&mut self) -> bool {
        !self.scroll.update_animation()
    }

    /// Check if scrolling
    pub fn is_scroll_animating(&self) -> bool {
        self.scroll.is_animating()
    }

    /// Ensure cursor is visible
    pub fn ensure_cursor_visible(&mut self) {
        let Some(viewport_lines) = self.viewport_lines else {
            return;
        };

        let cursor_row = self.cursor.position().row as f32;
        let scroll_y = self.scroll.position.y;
        let last_visible_line = scroll_y + viewport_lines;

        const PADDING: f32 = 3.0;

        if cursor_row < scroll_y {
            let target_scroll_y = (cursor_row - viewport_lines + PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else if cursor_row >= last_visible_line {
            let target_scroll_y = (cursor_row - PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }

    /// Load a file into the buffer.
    ///
    /// Reads file content, detects language from extension, updates buffer,
    /// and reparses for syntax highlighting.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to file to load
    /// * `cx` - GPUI context
    ///
    /// # Errors
    ///
    /// Returns error if file cannot be read.
    pub fn load_file(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;

        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(stoat_text::Language::from_extension)
            .unwrap_or(stoat_text::Language::PlainText);

        let path_buf = path.to_path_buf();

        // Check if buffer already exists in BufferStore, or create new one
        let (buffer_id, buffer_item_entity) = self
            .buffer_store
            .update(cx, |store, cx| {
                if let Some(buffer_item) = store.get_buffer_by_path(&path_buf) {
                    // Buffer exists, get its ID and return it
                    let buffer_id = buffer_item.read(cx).buffer().read(cx).remote_id();
                    Some((buffer_id, buffer_item))
                } else {
                    // Create new buffer in BufferStore
                    store.open_buffer(Some(path_buf.clone()), language, cx)
                }
            })
            .ok_or_else(|| "Failed to create buffer".to_string())?;

        // Update the buffer content
        buffer_item_entity.update(cx, |item, cx| {
            item.buffer().update(cx, |buffer, _| {
                let len = buffer.len();
                buffer.edit([(0..len, contents.as_str())]);
            });
            let _ = item.reparse(cx);
        });

        // Store strong reference in open_buffers if not already present
        if !self
            .open_buffers
            .iter()
            .any(|item| item.read(cx).buffer().read(cx).remote_id() == buffer_id)
        {
            self.open_buffers.push(buffer_item_entity.clone());
        }

        // Compute git diff
        buffer_item_entity.update(cx, |item, cx| {
            if let Ok(repo) = Repository::discover(path) {
                if let Ok(head_content) = repo.head_content(path) {
                    let buffer_snapshot = item.buffer().read(cx).snapshot();
                    let buffer_id = buffer_snapshot.remote_id();
                    match BufferDiff::new(buffer_id, head_content, &buffer_snapshot) {
                        Ok(diff) => {
                            item.set_diff(Some(diff));
                        },
                        Err(e) => {
                            tracing::error!("Failed to compute diff for {:?}: {}", path, e);
                        },
                    }
                }
            }
        });

        // Update active_buffer_id
        self.active_buffer_id = Some(buffer_id);

        // Update current file path for status bar
        self.current_file_path = Some(path_buf);

        self.cursor.move_to(text::Point::new(0, 0));
        cx.notify();

        Ok(())
    }

    /// Create a Stoat instance for testing with an empty buffer.
    ///
    /// Returns a [`TestStoat`] wrapper that provides test-oriented helper methods.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stoat = Stoat::test(cx);
    /// stoat.update(|s, cx| s.insert_text("hello", cx));
    /// assert_eq!(stoat.buffer_text(), "hello");
    /// ```
    #[cfg(test)]
    pub fn test(cx: &mut gpui::TestAppContext) -> crate::test::TestStoat<'_> {
        crate::test::TestStoat::new("", cx)
    }

    /// Create a Stoat instance for testing with specific buffer content.
    ///
    /// Returns a [`TestStoat`] wrapper that provides test-oriented helper methods.
    ///
    /// # Arguments
    ///
    /// * `text` - Initial buffer content
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stoat = Stoat::test_with_text("hello\nworld", cx);
    /// assert_eq!(stoat.buffer_text(), "hello\nworld");
    /// ```
    #[cfg(test)]
    pub fn test_with_text<'a>(
        text: &str,
        cx: &'a mut gpui::TestAppContext,
    ) -> crate::test::TestStoat<'a> {
        crate::test::TestStoat::new(text, cx)
    }
}
