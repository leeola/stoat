//! Application state management.
//!
//! Defines [`AppState`] which holds all application-level state that should be
//! shared across all panes and view types. This includes:
//!
//! - File system navigation (worktree)
//! - Buffer management (BufferStore)
//! - Workspace-wide modals (file finder, command palette, buffer finder)
//! - Version control (git status, diff review)
//!
//! # Architecture
//!
//! Application state is stored in [`PaneGroupView`](crate::pane_group::PaneGroupView) and
//! accessed by application-level actions. Individual views (EditorView, ImageView, etc.)
//! do NOT have direct access to application state - they communicate through actions
//! that are handled by PaneGroupView.
//!
//! # Separation of Concerns
//!
//! - **Workspace State** (this module): Shared across all views, lives in PaneGroupView
//! - **View State**: Per-instance UI state (cursor, scroll, zoom), lives in each view entity
//! - **Data State**: Content data (BufferItem, ImageItem), managed by stores
//!
//! See [`MULTI_VIEW_ARCHITECTURE.md`](../../MULTI_VIEW_ARCHITECTURE.md) for details.

use crate::{buffer::store::BufferStore, stoat::KeyContext, worktree::Worktree};
use gpui::{AppContext, Entity, Task};
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use text::Buffer;

/// File finder state.
///
/// Contains all state for the file finder modal including input buffer,
/// file list, filtering results, and preview.
pub struct FileFinder {
    /// Input buffer for search query
    pub input: Option<Entity<Buffer>>,
    /// All files in worktree
    pub files: Vec<crate::worktree::Entry>,
    /// Filtered file list (fuzzy matched against input)
    pub filtered: Vec<PathBuf>,
    /// Selected index in filtered list
    pub selected: usize,
    /// Previous mode to restore when closing finder
    pub previous_mode: Option<String>,
    /// Previous key context to restore when closing finder
    pub previous_key_context: Option<KeyContext>,
    /// File preview data (syntax-highlighted content)
    pub preview: Option<crate::file_finder::PreviewData>,
    /// Task loading preview in background
    pub preview_task: Option<Task<()>>,
}

impl Default for FileFinder {
    fn default() -> Self {
        Self {
            input: None,
            files: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            previous_mode: None,
            previous_key_context: None,
            preview: None,
            preview_task: None,
        }
    }
}

/// Buffer finder state.
///
/// Contains all state for the buffer finder modal which allows switching
/// between open buffers via fuzzy search.
pub struct BufferFinder {
    /// Input buffer for search query
    pub input: Option<Entity<Buffer>>,
    /// All open buffers
    pub buffers: Vec<crate::buffer::store::BufferListEntry>,
    /// Filtered buffer list (fuzzy matched against input)
    pub filtered: Vec<crate::buffer::store::BufferListEntry>,
    /// Selected index in filtered list
    pub selected: usize,
    /// Previous mode to restore when closing finder
    pub previous_mode: Option<String>,
    /// Previous key context to restore when closing finder
    pub previous_key_context: Option<KeyContext>,
}

impl Default for BufferFinder {
    fn default() -> Self {
        Self {
            input: None,
            buffers: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            previous_mode: None,
            previous_key_context: None,
        }
    }
}

/// Command palette state.
///
/// Contains all state for the command palette modal which provides fuzzy
/// searchable access to all editor commands/actions.
pub struct CommandPalette {
    /// Input buffer for search query
    pub input: Option<Entity<Buffer>>,
    /// All available commands
    pub commands: Vec<crate::CommandInfo>,
    /// Filtered command list (fuzzy matched against input)
    pub filtered: Vec<crate::CommandInfo>,
    /// Selected index in filtered list
    pub selected: usize,
    /// Previous mode to restore when closing palette
    pub previous_mode: Option<String>,
    /// Previous key context to restore when closing palette
    pub previous_key_context: Option<KeyContext>,
    /// Whether to show hidden commands
    pub show_hidden: bool,
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self {
            input: None,
            commands: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            previous_mode: None,
            previous_key_context: None,
            show_hidden: false,
        }
    }
}

/// Git status state.
///
/// Contains all state for the git status modal which displays modified
/// files with their status (modified, added, deleted, etc.).
pub struct GitStatus {
    /// All files with git changes
    pub files: Vec<crate::git::status::GitStatusEntry>,
    /// Filtered file list based on status filter
    pub filtered: Vec<crate::git::status::GitStatusEntry>,
    /// Current status filter (all, modified, staged, etc.)
    pub filter: crate::git::status::GitStatusFilter,
    /// Selected index in filtered list
    pub selected: usize,
    /// Previous mode to restore when closing modal
    pub previous_mode: Option<String>,
    /// Previous key context to restore when closing modal
    pub previous_key_context: Option<KeyContext>,
    /// Diff preview data for selected file
    pub preview: Option<crate::git::status::DiffPreviewData>,
    /// Task loading preview in background
    pub preview_task: Option<Task<()>>,
    /// Git branch information (name, ahead/behind counts)
    pub branch_info: Option<crate::git::status::GitBranchInfo>,
    /// Number of dirty files in working tree
    pub dirty_count: usize,
}

impl Default for GitStatus {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            filtered: Vec::new(),
            filter: crate::git::status::GitStatusFilter::default(),
            selected: 0,
            previous_mode: None,
            previous_key_context: None,
            preview: None,
            preview_task: None,
            branch_info: None,
            dirty_count: 0,
        }
    }
}

impl GitStatus {
    /// Filter files based on the current filter setting.
    ///
    /// Applies the current [`GitStatusFilter`](crate::git::status::GitStatusFilter) to
    /// the given file list and returns filtered results.
    ///
    /// # Arguments
    ///
    /// * `files` - List of all files to filter
    ///
    /// # Returns
    ///
    /// Vector of filtered files that match the current filter
    fn filter_files(
        &self,
        files: &[crate::git::status::GitStatusEntry],
    ) -> Vec<crate::git::status::GitStatusEntry> {
        files
            .iter()
            .filter(|entry| self.filter.matches(entry))
            .cloned()
            .collect()
    }
}

/// Diff review state.
///
/// Contains all state for diff review mode which allows reviewing and
/// approving git diff hunks across multiple files.
pub struct DiffReview {
    /// List of files with changes to review
    pub files: Vec<PathBuf>,
    /// Current file index being reviewed
    pub current_file_idx: usize,
    /// Current hunk index within file being reviewed
    pub current_hunk_idx: usize,
    /// Approved hunks by file (used for selective staging)
    pub approved_hunks: HashMap<PathBuf, std::collections::HashSet<usize>>,
    /// Previous mode to restore when exiting review
    pub previous_mode: Option<String>,
    /// Comparison mode (working vs HEAD, working vs index, index vs HEAD)
    pub comparison_mode: crate::git::diff_review::DiffComparisonMode,
}

impl Default for DiffReview {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            current_file_idx: 0,
            current_hunk_idx: 0,
            approved_hunks: HashMap::new(),
            previous_mode: None,
            comparison_mode: crate::git::diff_review::DiffComparisonMode::default(),
        }
    }
}

/// Application-level state shared across all panes and view types.
///
/// This struct contains all state that should be accessible from any view
/// regardless of type (text editor, image viewer, table viewer, etc.).
/// Application state is stored in [`PaneGroupView`](crate::pane_group::PaneGroupView)
/// and accessed by application-level actions.
///
/// # Usage in PaneGroupView
///
/// Workspace actions (file finder, command palette, etc.) operate on this
/// state directly without needing to know which view type is active:
///
/// ```rust,ignore
/// fn handle_open_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
///     // No longer need to check if active pane is EditorView!
///     self.app_state.file_finder.input = Some(cx.new(|_| Buffer::new(...)));
///     // ... rest of file finder logic
/// }
/// ```
///
/// # Relationship to View State
///
/// Individual views maintain their own state (cursor, scroll, zoom, etc.)
/// but do NOT duplicate application state. For example:
///
/// - [`EditorView`](crate::editor::view::EditorView) has cursor position and selections
/// - `ImageView` has zoom level and pan offset
/// - But both access the same worktree and buffer_store through app state
///
/// # Initialization
///
/// Application state is initialized when creating
/// [`PaneGroupView`](crate::pane_group::PaneGroupView):
///
/// ```rust,ignore
/// let workspace = AppState::new(cx);
/// let pane_group = cx.new(|cx| PaneGroupView::new(workspace, cx));
/// ```
pub struct AppState {
    /// File system tree for navigation
    pub worktree: Arc<Mutex<Worktree>>,
    /// Central buffer management (tracks all open buffers)
    pub buffer_store: Entity<BufferStore>,
    /// File finder modal state
    pub file_finder: FileFinder,
    /// Buffer finder modal state
    pub buffer_finder: BufferFinder,
    /// Command palette modal state
    pub command_palette: CommandPalette,
    /// Git status modal state
    pub git_status: GitStatus,
    /// Diff review mode state
    pub diff_review: DiffReview,
}

impl AppState {
    /// Create new application state.
    ///
    /// Initializes application state with a worktree rooted at the current directory
    /// and empty buffer store. Loads initial git status if in a git repository.
    ///
    /// # Arguments
    ///
    /// * `cx` - GPUI context for entity creation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let workspace = AppState::new(cx);
    /// ```
    pub fn new(cx: &mut gpui::App) -> Self {
        let worktree = Arc::new(Mutex::new(Worktree::new(PathBuf::from("."))));
        let buffer_store = cx.new(|_| BufferStore::new());

        // Initialize git status for status bar
        let (branch_info, git_status_files, dirty_count) =
            if let Ok(repo) = crate::git::repository::Repository::open(std::path::Path::new(".")) {
                let branch_info = crate::git::status::gather_git_branch_info(repo.inner());
                let status_files = crate::git::status::gather_git_status(repo.inner())
                    .unwrap_or_else(|_| Vec::new());
                let dirty_count = status_files.len();
                (branch_info, status_files, dirty_count)
            } else {
                (None, Vec::new(), 0)
            };

        Self {
            worktree,
            buffer_store,
            file_finder: FileFinder::default(),
            buffer_finder: BufferFinder::default(),
            command_palette: CommandPalette::default(),
            git_status: GitStatus {
                files: git_status_files.clone(),
                filtered: git_status_files,
                branch_info,
                dirty_count,
                ..Default::default()
            },
            diff_review: DiffReview::default(),
        }
    }

    /// Open file finder modal.
    ///
    /// Initializes the file finder with all files from the worktree and creates
    /// an input buffer for search queries. Returns the previous mode and key_context
    /// that should be restored when the finder is dismissed.
    ///
    /// The caller (typically [`PaneGroupView`](crate::pane_group::PaneGroupView))
    /// is responsible for:
    /// - Setting the active editor's key_context to [`KeyContext::FileFinder`]
    /// - Setting the active editor's mode to "file_finder"
    /// - Loading the preview for the first file
    ///
    /// # Arguments
    ///
    /// * `current_mode` - Current mode to save for restoration
    /// * `current_key_context` - Current key context to save for restoration
    /// * `cx` - GPUI context for entity creation
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` tuple for restoration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (prev_mode, prev_ctx) = self.app_state.open_file_finder(mode, key_context, cx);
    /// editor.stoat.update(cx, |stoat, cx| {
    ///     stoat.set_key_context(KeyContext::FileFinder);
    ///     stoat.set_mode("file_finder");
    /// });
    /// ```
    pub fn open_file_finder(
        &mut self,
        current_mode: String,
        current_key_context: KeyContext,
        cx: &mut gpui::App,
    ) -> (Option<String>, Option<KeyContext>) {
        use std::num::NonZeroU64;
        use text::BufferId;

        // Save current state for restoration
        self.file_finder.previous_mode = Some(current_mode);
        self.file_finder.previous_key_context = Some(current_key_context);

        // Create input buffer with BufferId 2 (following existing convention)
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.file_finder.input = Some(input_buffer);

        // Scan worktree for files
        let entries = self.worktree.lock().snapshot().entries(false);
        self.file_finder.files = entries.clone();
        self.file_finder.filtered = entries
            .iter()
            .map(|e| PathBuf::from(e.path.as_unix_str()))
            .collect();
        self.file_finder.selected = 0;

        // Clear any existing preview (caller will load new preview)
        self.file_finder.preview = None;
        self.file_finder.preview_task = None;

        (
            self.file_finder.previous_mode.clone(),
            self.file_finder.previous_key_context,
        )
    }

    /// Open command palette modal.
    ///
    /// Builds a list of all available commands from action metadata and creates
    /// an input buffer for fuzzy search. The command palette provides a searchable
    /// interface to all registered actions in the editor.
    ///
    /// The caller (typically [`PaneGroupView`](crate::pane_group::PaneGroupView))
    /// is responsible for:
    /// - Setting the active view's key_context to [`KeyContext::CommandPalette`]
    /// - Setting the active view's mode to "command_palette"
    ///
    /// # Arguments
    ///
    /// * `current_mode` - Current mode to save for restoration
    /// * `current_key_context` - Current key context to save for restoration
    /// * `cx` - GPUI context for entity creation
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` tuple for restoration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (prev_mode, prev_ctx) = self.app_state.open_command_palette(mode, key_context, cx);
    /// editor.stoat.update(cx, |stoat, cx| {
    ///     stoat.set_key_context(KeyContext::CommandPalette);
    ///     stoat.set_mode("command_palette");
    /// });
    /// ```
    pub fn open_command_palette(
        &mut self,
        current_mode: String,
        current_key_context: KeyContext,
        cx: &mut gpui::App,
    ) -> (Option<String>, Option<KeyContext>) {
        use std::num::NonZeroU64;
        use text::BufferId;

        // Save current state for restoration
        self.command_palette.previous_mode = Some(current_mode);
        self.command_palette.previous_key_context = Some(current_key_context);

        // Build command list from action metadata
        let commands = crate::stoat_actions::build_command_list();

        // Create input buffer with BufferId 3 (following existing convention)
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.command_palette.input = Some(input_buffer);

        // Initialize command palette state
        self.command_palette.commands = commands.clone();
        self.command_palette.filtered = commands;
        self.command_palette.selected = 0;

        (
            self.command_palette.previous_mode.clone(),
            self.command_palette.previous_key_context,
        )
    }

    /// Dismiss command palette and restore previous mode/context.
    ///
    /// Returns the mode and key_context to restore, or None if command palette
    /// wasn't open.
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` to restore, or `(None, None)` if not open
    pub fn dismiss_command_palette(&mut self) -> (Option<String>, Option<KeyContext>) {
        let prev_mode = self.command_palette.previous_mode.take();
        let prev_ctx = self.command_palette.previous_key_context.take();

        // Clear command palette state
        self.command_palette.input = None;
        self.command_palette.commands.clear();
        self.command_palette.filtered.clear();
        self.command_palette.selected = 0;
        self.command_palette.show_hidden = false;

        (prev_mode, prev_ctx)
    }

    /// Open buffer finder modal.
    ///
    /// Retrieves all open buffers from [`BufferStore`] and creates an input buffer
    /// for fuzzy search. The buffer finder provides quick navigation between all
    /// currently open buffers.
    ///
    /// The caller (typically [`PaneGroupView`](crate::pane_group::PaneGroupView))
    /// is responsible for:
    /// - Setting the active view's key_context to [`KeyContext::BufferFinder`]
    /// - Setting the active view's mode to "buffer_finder"
    ///
    /// # Arguments
    ///
    /// * `current_mode` - Current mode to save for restoration
    /// * `current_key_context` - Current key context to save for restoration
    /// * `cx` - GPUI context for entity creation
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` tuple for restoration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (prev_mode, prev_ctx) = self.app_state.open_buffer_finder(mode, key_context, cx);
    /// editor.stoat.update(cx, |stoat, cx| {
    ///     stoat.set_key_context(KeyContext::BufferFinder);
    ///     stoat.set_mode("buffer_finder");
    /// });
    /// ```
    pub fn open_buffer_finder(
        &mut self,
        current_mode: String,
        current_key_context: KeyContext,
        cx: &mut gpui::App,
    ) -> (Option<String>, Option<KeyContext>) {
        use std::num::NonZeroU64;
        use text::BufferId;

        // Save current state for restoration
        self.buffer_finder.previous_mode = Some(current_mode);
        self.buffer_finder.previous_key_context = Some(current_key_context);

        // Create input buffer with BufferId 4 (following existing convention)
        let buffer_id = BufferId::from(NonZeroU64::new(4).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.buffer_finder.input = Some(input_buffer);

        // Get all open buffers from buffer_store (caller will update active/visible status)
        let buffers = self.buffer_store.read(cx).buffer_list(None, &[], cx);
        self.buffer_finder.buffers = buffers.clone();
        self.buffer_finder.filtered = buffers;
        self.buffer_finder.selected = 0;

        (
            self.buffer_finder.previous_mode.clone(),
            self.buffer_finder.previous_key_context,
        )
    }

    /// Dismiss buffer finder and restore previous mode/context.
    ///
    /// Returns the mode and key_context to restore, or None if buffer finder
    /// wasn't open.
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` to restore, or `(None, None)` if not open
    pub fn dismiss_buffer_finder(&mut self) -> (Option<String>, Option<KeyContext>) {
        let prev_mode = self.buffer_finder.previous_mode.take();
        let prev_ctx = self.buffer_finder.previous_key_context.take();

        // Clear buffer finder state
        self.buffer_finder.input = None;
        self.buffer_finder.buffers.clear();
        self.buffer_finder.filtered.clear();
        self.buffer_finder.selected = 0;

        (prev_mode, prev_ctx)
    }

    /// Open git status modal.
    ///
    /// Refreshes git status from the repository and applies the current filter.
    /// Creates the git status modal showing all files with changes and their status.
    ///
    /// The caller (typically [`PaneGroupView`](crate::pane_group::PaneGroupView))
    /// is responsible for:
    /// - Setting the active view's key_context to [`KeyContext::Git`]
    /// - Setting the active view's mode to "git_status"
    ///
    /// # Arguments
    ///
    /// * `current_mode` - Current mode to save for restoration
    /// * `current_key_context` - Current key context to save for restoration
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` tuple for restoration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (prev_mode, prev_ctx) = self.app_state.open_git_status(mode, key_context);
    /// editor.stoat.update(cx, |stoat, cx| {
    ///     stoat.set_key_context(KeyContext::Git);
    ///     stoat.set_mode("git_status");
    /// });
    /// ```
    pub fn open_git_status(
        &mut self,
        current_mode: String,
        current_key_context: KeyContext,
    ) -> (Option<String>, Option<KeyContext>) {
        // Save current state for restoration
        self.git_status.previous_mode = Some(current_mode);
        self.git_status.previous_key_context = Some(current_key_context);

        // Refresh git status from repository
        if let Ok(repo) = crate::git::repository::Repository::open(std::path::Path::new(".")) {
            let branch_info = crate::git::status::gather_git_branch_info(repo.inner());
            let status_files =
                crate::git::status::gather_git_status(repo.inner()).unwrap_or_else(|_| Vec::new());
            let dirty_count = status_files.len();

            self.git_status.files = status_files.clone();
            self.git_status.branch_info = branch_info;
            self.git_status.dirty_count = dirty_count;
        } else {
            self.git_status.files.clear();
            self.git_status.branch_info = None;
            self.git_status.dirty_count = 0;
        }

        // Apply current filter
        self.git_status.filtered = self.git_status.filter_files(&self.git_status.files);
        self.git_status.selected = 0;

        // Clear any existing preview (caller will load new preview)
        self.git_status.preview = None;
        self.git_status.preview_task = None;

        (
            self.git_status.previous_mode.clone(),
            self.git_status.previous_key_context,
        )
    }

    /// Dismiss git status modal and restore previous mode/context.
    ///
    /// Returns the mode and key_context to restore, or None if git status
    /// wasn't open.
    ///
    /// # Returns
    ///
    /// `(previous_mode, previous_key_context)` to restore, or `(None, None)` if not open
    pub fn dismiss_git_status(&mut self) -> (Option<String>, Option<KeyContext>) {
        let prev_mode = self.git_status.previous_mode.take();
        let prev_ctx = self.git_status.previous_key_context.take();

        // Clear git status modal state (but keep files and branch_info for status bar)
        self.git_status.filtered.clear();
        self.git_status.selected = 0;
        self.git_status.preview = None;
        self.git_status.preview_task = None;

        (prev_mode, prev_ctx)
    }
}
