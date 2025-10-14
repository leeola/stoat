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
use gpui::{App, AppContext, Context, Entity, EventEmitter, Task, WeakEntity};
use nucleo_matcher::{Config, Matcher};
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use stoat_text::Language;
use text::{Buffer, BufferId, Point};

/// KeyContext for keybinding dispatch.
///
/// Represents the high-level context determining which UI is active.
/// Mapped to GPUI's [`gpui::KeyContext`] for binding resolution.
///
/// Following Zed's pattern, these contexts group related bindings and determine
/// which modal/UI is rendered. Multiple modes can exist within the same context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyContext {
    /// Text editing context (normal, insert, visual modes)
    TextEditor,
    /// Git status modal context (git_status, git_filter modes)
    Git,
    /// File finder modal context
    FileFinder,
    /// Buffer finder modal context
    BufferFinder,
    /// Command palette modal context
    CommandPalette,
    /// Diff review mode context
    DiffReview,
    /// Help modal context
    HelpModal,
}

impl KeyContext {
    /// Get the string representation for GPUI KeyContext.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TextEditor => "TextEditor",
            Self::Git => "Git",
            Self::FileFinder => "FileFinder",
            Self::BufferFinder => "BufferFinder",
            Self::CommandPalette => "CommandPalette",
            Self::DiffReview => "DiffReview",
            Self::HelpModal => "HelpModal",
        }
    }

    /// Parse a KeyContext from string, validating it's a known context.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "TextEditor" => Ok(Self::TextEditor),
            "Git" => Ok(Self::Git),
            "FileFinder" => Ok(Self::FileFinder),
            "BufferFinder" => Ok(Self::BufferFinder),
            "CommandPalette" => Ok(Self::CommandPalette),
            "DiffReview" => Ok(Self::DiffReview),
            "HelpModal" => Ok(Self::HelpModal),
            _ => Err(format!("Unknown KeyContext: {}", s)),
        }
    }
}

/// Metadata for a KeyContext.
///
/// Associates a KeyContext with its default mode. When entering a context via
/// [`SetKeyContext`](crate::actions::SetKeyContext), the mode is automatically
/// set to the default mode specified in the keymap config.
#[derive(Clone, Debug)]
pub struct KeyContextMeta {
    /// Default mode when entering this context
    pub default_mode: String,
}

impl KeyContextMeta {
    /// Create new KeyContext metadata.
    pub fn new(default_mode: String) -> Self {
        Self { default_mode }
    }
}

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
    /// Command aliases (e.g., ["q", "quit"] for QuitApp)
    pub aliases: Vec<&'static str>,
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

    /// Current KeyContext (controls UI rendering and keybinding groups)
    pub(crate) key_context: KeyContext,

    /// Registry of KeyContexts with their metadata
    pub(crate) contexts: HashMap<KeyContext, KeyContextMeta>,

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
    pub(crate) buffer_finder_buffers: Vec<crate::buffer_store::BufferListEntry>,
    pub(crate) buffer_finder_filtered: Vec<crate::buffer_store::BufferListEntry>,
    pub(crate) buffer_finder_selected: usize,
    pub(crate) buffer_finder_previous_mode: Option<String>,

    // Git status state
    pub(crate) git_status_files: Vec<crate::git_status::GitStatusEntry>,
    pub(crate) git_status_filtered: Vec<crate::git_status::GitStatusEntry>,
    pub(crate) git_status_filter: crate::git_status::GitStatusFilter,
    pub(crate) git_status_selected: usize,
    pub(crate) git_status_previous_mode: Option<String>,
    pub(crate) git_status_preview: Option<crate::git_status::DiffPreviewData>,
    pub(crate) git_status_preview_task: Option<Task<()>>,

    // Help modal state
    pub(crate) help_modal_previous_mode: Option<String>,
    pub(crate) git_status_branch_info: Option<crate::git_status::GitBranchInfo>,
    pub(crate) git_dirty_count: usize,

    // Diff review state
    pub(crate) diff_review_files: Vec<crate::diff_review::DiffReviewFile>,
    pub(crate) diff_review_current_file_idx: usize,
    pub(crate) diff_review_current_hunk_idx: usize,
    pub(crate) diff_review_approved_hunks:
        std::collections::HashMap<PathBuf, std::collections::HashSet<usize>>,
    pub(crate) diff_review_previous_mode: Option<String>,

    /// Current file path (for status bar display)
    pub(crate) current_file_path: Option<PathBuf>,

    /// Worktree for file scanning
    pub(crate) worktree: Arc<Mutex<Worktree>>,

    /// Parent stoat when this is a minimap instance.
    ///
    /// When `Some`, this Stoat is acting as a minimap for the parent editor.
    /// Following Zed's pattern, the minimap is just another Stoat instance with
    /// tiny font and this parent reference to synchronize scroll and handle interactions.
    pub(crate) parent_stoat: Option<WeakEntity<Stoat>>,
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

        // Initialize KeyContext registry from keymap.toml
        let contexts = crate::keymap::parse_contexts_from_config();
        let key_context = KeyContext::TextEditor; // Default context

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
            key_context,
            contexts,
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
            git_status_files: git_status_files.clone(),
            git_status_filtered: git_status_files,
            git_status_filter: crate::git_status::GitStatusFilter::default(),
            git_status_selected: 0,
            git_status_previous_mode: None,
            git_status_preview: None,
            git_status_preview_task: None,
            help_modal_previous_mode: None,
            git_status_branch_info: git_branch_info,
            git_dirty_count,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            current_file_path: None,
            worktree,
            parent_stoat: None,
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
            key_context: self.key_context,
            contexts: self.contexts.clone(),
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
            git_status_filtered: self.git_status_filtered.clone(),
            git_status_filter: self.git_status_filter,
            git_status_selected: 0,
            git_status_previous_mode: None,
            git_status_preview: None,
            git_status_preview_task: None,
            help_modal_previous_mode: None,
            git_status_branch_info: self.git_status_branch_info.clone(),
            git_dirty_count: self.git_dirty_count,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            current_file_path: self.current_file_path.clone(),
            worktree: self.worktree.clone(),
            parent_stoat: None,
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
    ///
    /// Following Zed's architecture: if this is a minimap, delegates to the parent editor
    /// to get the active buffer. This ensures the minimap always shows the parent's current buffer.
    pub fn active_buffer(&self, cx: &App) -> Entity<BufferItem> {
        // If this is a minimap, delegate to parent (following Zed's pattern)
        if let Some(parent_weak) = &self.parent_stoat {
            if let Some(parent) = parent_weak.upgrade() {
                return parent.read(cx).active_buffer(cx);
            }
        }

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
                    "No active buffer - weak reference upgrade failed for buffer_id {buffer_id:?}"
                );
            },
        }
    }

    /// Get the currently active buffer ID.
    ///
    /// Returns the [`BufferId`] of the active buffer, or `None` if no buffer is active.
    ///
    /// Following Zed's architecture: if this is a minimap, delegates to the parent editor.
    pub fn active_buffer_id(&self, cx: &App) -> Option<BufferId> {
        // If this is a minimap, delegate to parent (following Zed's pattern)
        if let Some(parent_weak) = &self.parent_stoat {
            if let Some(parent) = parent_weak.upgrade() {
                return parent.read(cx).active_buffer_id(cx);
            }
        }

        self.active_buffer_id
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

    /// Set scroll position (used by minimap synchronization)
    pub fn set_scroll_position(&mut self, position: gpui::Point<f32>) {
        self.scroll.position = position;
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

    /// Get current KeyContext.
    ///
    /// Returns the active [`KeyContext`] controlling UI rendering and keybinding groups.
    pub fn key_context(&self) -> KeyContext {
        self.key_context
    }

    /// Set KeyContext.
    ///
    /// Changes the active [`KeyContext`], which controls which UI is rendered
    /// (e.g., TextEditor vs Git modal) and which keybinding groups are active.
    pub fn set_key_context(&mut self, context: KeyContext) {
        self.key_context = context;
    }

    /// Get KeyContext metadata by context.
    ///
    /// Returns the [`KeyContextMeta`] containing default mode for the given context,
    /// or `None` if the context is not registered.
    pub fn get_key_context_meta(&self, context: KeyContext) -> Option<&KeyContextMeta> {
        self.contexts.get(&context)
    }

    /// Check if this is a minimap instance.
    ///
    /// Returns `true` if this Stoat is acting as a minimap for a parent editor.
    /// Minimap instances render with tiny font and synchronize with the parent's scroll.
    pub fn is_minimap(&self) -> bool {
        self.parent_stoat.is_some()
    }

    /// Check if we're currently in diff review mode.
    ///
    /// Returns `true` if diff review mode is active. Since we persist state when
    /// exiting review mode (for position restoration), we check both mode and files.
    /// Used by GUI to adjust gutter width and show diff backgrounds.
    pub fn is_in_diff_review(&self) -> bool {
        self.mode == "diff_review" && !self.diff_review_files.is_empty()
    }

    /// Get diff review progress as (reviewed_count, total_count).
    ///
    /// Returns [`None`] if not in review mode. Used by status bar to show progress like "5/30".
    ///
    /// # Returns
    ///
    /// `Some((reviewed, total))` where:
    /// - `reviewed`: Number of approved hunks across all files
    /// - `total`: Total number of hunks across all files
    pub fn diff_review_progress(&self) -> Option<(usize, usize)> {
        if self.diff_review_files.is_empty() {
            return None;
        }

        let total: usize = self.diff_review_files.iter().map(|f| f.hunk_count).sum();
        let reviewed: usize = self
            .diff_review_approved_hunks
            .values()
            .map(|set| set.len())
            .sum();

        Some((reviewed, total))
    }

    /// Get current file progress in review as (current_file, total_files).
    ///
    /// Returns [`None`] if not in review mode. Used by status bar to show progress like "File 2/5".
    ///
    /// # Returns
    ///
    /// `Some((current, total))` where both are 1-indexed for display
    pub fn diff_review_file_progress(&self) -> Option<(usize, usize)> {
        if self.diff_review_files.is_empty() {
            return None;
        }

        Some((
            self.diff_review_current_file_idx + 1, // 1-indexed for display
            self.diff_review_files.len(),
        ))
    }

    /// Get the parent stoat if this is a minimap.
    ///
    /// Returns the parent editor entity if this is a minimap, or `None` for regular editors.
    pub fn parent_stoat(&self) -> Option<&WeakEntity<Stoat>> {
        self.parent_stoat.as_ref()
    }

    /// Get viewport height in lines
    pub fn viewport_lines(&self) -> Option<f32> {
        self.viewport_lines
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

        if cursor_row < scroll_y + PADDING {
            // Scrolling up: position cursor PADDING lines from top
            let target_scroll_y = (cursor_row - PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else if cursor_row >= last_visible_line - PADDING {
            // Scrolling down: position cursor PADDING lines from bottom
            let target_scroll_y = (cursor_row - viewport_lines + PADDING + 1.0).max(0.0);
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
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {e}"))?;

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
            // Set saved text baseline for modification tracking
            item.set_saved_text(contents.clone());
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

    /// Create a minimap instance for this editor.
    ///
    /// Following Zed's architecture, the minimap is just another [`Stoat`] instance
    /// that shares the same buffer but renders with a tiny font. The minimap tracks
    /// the parent editor via a weak reference for scroll synchronization.
    ///
    /// The GUI layer will apply minimap-specific styling (tiny font, bold weight).
    ///
    /// # Arguments
    ///
    /// * `cx` - GPUI context
    ///
    /// # Returns
    ///
    /// A new [`Stoat`] entity configured as a minimap
    pub fn create_minimap(&self, cx: &mut Context<Self>) -> Entity<Self> {
        // Get weak reference to parent (self)
        let parent_weak = cx.weak_entity();

        // Create minimap in a special mode
        cx.new(|_cx| Self {
            buffer_store: self.buffer_store.clone(),
            open_buffers: self.open_buffers.clone(),
            active_buffer_id: self.active_buffer_id,
            cursor: CursorManager::new(), // New cursor for minimap
            scroll: self.scroll.clone(),  // Clone scroll state (will be synced)
            viewport_lines: None,         // Will be set by layout
            mode: "minimap".into(),       // Special mode for minimap
            modes: self.modes.clone(),
            key_context: KeyContext::TextEditor, // Minimap always in editor context
            contexts: self.contexts.clone(),
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
            git_status_files: Vec::new(),
            git_status_filtered: Vec::new(),
            git_status_filter: crate::git_status::GitStatusFilter::default(),
            git_status_selected: 0,
            git_status_previous_mode: None,
            git_status_preview: None,
            git_status_preview_task: None,
            help_modal_previous_mode: None,
            git_status_branch_info: None,
            git_dirty_count: 0,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            current_file_path: None,
            worktree: self.worktree.clone(),
            parent_stoat: Some(parent_weak),
        })
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

    /// Create a Stoat instance for testing with cursor notation.
    ///
    /// Uses the cursor notation DSL to specify buffer content and initial
    /// cursor/selection positions in a single string.
    ///
    /// # Arguments
    ///
    /// * `marked_text` - Text with cursor/selection markers (see [`crate::test::cursor_notation`])
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Cursor at position 6
    /// let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
    /// assert_eq!(stoat.cursor_position(), Point::new(0, 6));
    ///
    /// // Selection with cursor at end
    /// let stoat = Stoat::test_with_cursor_notation("<|hello||>", cx).unwrap();
    /// assert_eq!(stoat.selection().range(), 0..5);
    /// ```
    #[cfg(test)]
    pub fn test_with_cursor_notation<'a>(
        marked_text: &str,
        cx: &'a mut gpui::TestAppContext,
    ) -> anyhow::Result<crate::test::TestStoat<'a>> {
        crate::test::TestStoat::test_with_cursor_notation(marked_text, cx)
    }
}
