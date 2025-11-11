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

use crate::{buffer::store::BufferStore, stoat::KeyContext, worktree::Worktree, BufferItem};
use gpui::{AppContext, Entity, Task};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use stoat_lsp::DiagnosticSet;
use text::Buffer;

/// Maximum UI notification rate for LSP progress updates (milliseconds).
///
/// Limits status bar re-renders to 5 per second during rust-analyzer indexing.
const PROGRESS_DEBOUNCE_MS: u64 = 200;

/// LSP server status.
#[derive(Clone, Debug, PartialEq)]
pub enum LspStatus {
    /// Server process starting
    Starting,
    /// Sending initialize request
    Initializing,
    /// Active operation (indexing, building, etc.)
    Indexing { operation: String },
    /// Idle and ready
    Ready,
    /// Error during startup or operation
    Error(String),
}

impl LspStatus {
    /// Display string for status bar (empty when Ready).
    pub fn display_string(&self) -> String {
        match self {
            LspStatus::Starting => "LSP: Starting...".to_string(),
            LspStatus::Initializing => "LSP: Initializing...".to_string(),
            LspStatus::Indexing { operation } => format!("LSP: {}", operation),
            LspStatus::Ready => String::new(),
            LspStatus::Error(msg) => format!("LSP: Error: {}", msg),
        }
    }
}

/// Progress info for a single operation.
#[derive(Clone, Debug)]
struct OperationProgress {
    title: String,
    percentage: Option<u32>,
}

/// LSP state tracking.
#[derive(Clone)]
pub struct LspState {
    /// Current status
    pub status: Arc<RwLock<LspStatus>>,
    /// Active operations (token -> progress info)
    active_operations: Arc<RwLock<HashMap<String, OperationProgress>>>,
}

impl Default for LspState {
    fn default() -> Self {
        Self {
            status: Arc::new(RwLock::new(LspStatus::Starting)),
            active_operations: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

/// File finder state.
///
/// Contains all state for the file finder modal including input buffer,
/// file list, filtering results, and preview.
#[derive(Default)]
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

/// Buffer finder state.
///
/// Contains all state for the buffer finder modal which allows switching
/// between open buffers via fuzzy search.
#[derive(Default)]
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

/// Command palette state.
///
/// Contains all state for the command palette modal which provides fuzzy
/// searchable access to all editor commands/actions.
#[derive(Default)]
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

/// Command line state for vim-style commands.
///
/// Contains state for the command line mode which allows entering vim-style
/// commands like `:cd`, `:w`, `:q`.
#[derive(Default)]
pub struct CommandLine {
    /// Input buffer for command text
    pub input: Option<Entity<Buffer>>,
    /// Previous mode to restore when closing command line
    pub previous_mode: Option<String>,
    /// Previous key context to restore when closing command line
    pub previous_key_context: Option<KeyContext>,
}

/// Git status state.
///
/// Contains all state for the git status modal which displays modified
/// files with their status (modified, added, deleted, etc.).
#[derive(Default)]
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
#[derive(Default)]
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
    /// CommandPaletteV2 entity (new entity-based palette)
    ///
    /// Entity-based command palette using InlineEditor. Created when opened,
    /// dropped when dismissed. None when not visible.
    pub command_palette_v2: Option<Entity<crate::command_palette_v2::CommandPaletteV2>>,
    /// Command line modal state
    pub command_line: CommandLine,
    /// Git status modal state
    pub git_status: GitStatus,
    /// Diff review mode state
    pub diff_review: DiffReview,
    /// LSP manager for language server coordination
    ///
    /// Manages language server processes and routes diagnostics to buffers.
    /// Currently requires manual diagnostic routing - automatic routing will be added later.
    pub lsp_manager: Arc<stoat_lsp::LspManager>,
    /// LSP state tracking (status and progress)
    pub lsp_state: LspState,
    /// Mode state shared by all TextEditor panes.
    ///
    /// All text editor panes in all splits share this mode state. When switching
    /// between text editor panes, they maintain the same mode (normal, insert, visual, etc.).
    pub text_editor_mode: String,
    /// Mode state shared by all InlineEditor instances.
    ///
    /// All inline editors (command palette, file finder inputs, etc.) share this mode state.
    /// Separate from text_editor_mode so opening a modal doesn't affect text editing mode.
    pub inline_editor_mode: String,
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

        let lsp_manager = Arc::new(stoat_lsp::LspManager::new(
            cx.background_executor().clone(),
            std::time::Duration::from_secs(120),
        ));
        let lsp_state = LspState::default();

        // Setup LSP background task for automatic diagnostic routing
        {
            let updates = lsp_manager.subscribe_diagnostic_updates();
            let lsp_manager_clone = lsp_manager.clone();
            let buffer_store_clone = buffer_store.clone();
            let diagnostics_version = Arc::new(AtomicU64::new(0));

            cx.spawn(async move |cx| {
                while let Ok(update) = updates.recv().await {
                    let path = update.path.clone();
                    let server_id = update.server_id;
                    let lsp_manager = lsp_manager_clone.clone();

                    // Generate monotonically increasing version number
                    let version = diagnostics_version.fetch_add(1, Ordering::SeqCst) + 1;

                    // Find BufferItem and update diagnostics on main thread
                    let result: Result<Option<(Entity<BufferItem>, DiagnosticSet)>, _> =
                        cx.update(|cx| {
                            let buffer_item =
                                buffer_store_clone.read(cx).get_buffer_by_path(&path)?;
                            let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
                            let diag_set = lsp_manager.diagnostics_for_buffer(&path, &snapshot)?;

                            Some((buffer_item.clone(), diag_set))
                        });
                    let result = result.ok().flatten();

                    if let Some((buffer_item, diag_set)) = result {
                        let _ = buffer_item
                            .update(cx, |item, cx| {
                                item.update_diagnostics(server_id, diag_set, version, cx);
                            })
                            .ok();
                    }
                }
            })
            .detach();
        }

        Self {
            worktree,
            buffer_store,
            file_finder: FileFinder::default(),
            buffer_finder: BufferFinder::default(),
            command_palette: CommandPalette::default(),
            command_palette_v2: None,
            command_line: CommandLine::default(),
            git_status: GitStatus {
                files: git_status_files.clone(),
                filtered: git_status_files,
                branch_info,
                dirty_count,
                ..Default::default()
            },
            diff_review: DiffReview::default(),
            lsp_manager,
            lsp_state,
            text_editor_mode: "normal".to_string(),
            inline_editor_mode: "normal".to_string(),
        }
    }

    /// Set up LSP progress tracking tasks.
    ///
    /// Spawns background tasks that update LSP status and notify the view to trigger re-renders.
    /// Must be called from an entity context to enable automatic UI updates.
    pub fn setup_lsp_progress_tracking<V: 'static>(
        &self,
        view: gpui::WeakEntity<V>,
        cx: &mut gpui::App,
    ) {
        // Subscribe to LSP progress notifications
        {
            let lsp_state_clone = self.lsp_state.clone();
            let progress_updates = self.lsp_manager.subscribe_progress_updates();
            let view_clone = view.clone();

            cx.spawn(async move |cx| {
                let mut last_notify: Option<Instant> = None;

                while let Ok(update) = progress_updates.recv().await {
                    let mut active_ops = lsp_state_clone.active_operations.write();

                    match update.kind {
                        stoat_lsp::ProgressKind::Begin => {
                            if !update.title.is_empty() {
                                active_ops.insert(
                                    update.token.clone(),
                                    OperationProgress {
                                        title: update.title.clone(),
                                        percentage: update.percentage,
                                    },
                                );
                            }
                        },
                        stoat_lsp::ProgressKind::Report => {
                            if !update.title.is_empty() {
                                active_ops.insert(
                                    update.token.clone(),
                                    OperationProgress {
                                        title: update.title.clone(),
                                        percentage: update.percentage,
                                    },
                                );
                            } else if let Some(progress) = active_ops.get_mut(&update.token) {
                                progress.percentage = update.percentage;
                            }
                        },
                        stoat_lsp::ProgressKind::End => {
                            active_ops.remove(&update.token);
                        },
                    }

                    let new_status = if active_ops.is_empty() {
                        LspStatus::Ready
                    } else {
                        let count = active_ops.len();
                        let operation = if count == 1 {
                            let progress = active_ops.values().next().unwrap();
                            if let Some(pct) = progress.percentage {
                                format!("{} {}%", progress.title, pct)
                            } else {
                                progress.title.clone()
                            }
                        } else {
                            let total_pct: Option<u32> = active_ops
                                .values()
                                .filter_map(|p| p.percentage)
                                .sum::<u32>()
                                .checked_div(count as u32);

                            if let Some(avg_pct) = total_pct {
                                format!("Indexing ({} tasks, {}%)", count, avg_pct)
                            } else {
                                format!("Indexing ({} tasks)", count)
                            }
                        };
                        LspStatus::Indexing { operation }
                    };

                    drop(active_ops);

                    let mut status_guard = lsp_state_clone.status.write();
                    if *status_guard != new_status {
                        match (&*status_guard, &new_status) {
                            (_, LspStatus::Initializing) => {
                                tracing::debug!("LSP initializing");
                            },
                            (_, LspStatus::Ready) => {
                                tracing::debug!("LSP ready");
                            },
                            (_, LspStatus::Error(_)) => {
                                tracing::debug!("LSP error: {:?}", new_status);
                            },
                            (LspStatus::Indexing { .. }, LspStatus::Indexing { .. }) => {
                                tracing::trace!("LSP indexing: {:?}", new_status);
                            },
                            (_, LspStatus::Indexing { .. }) => {
                                tracing::debug!("LSP indexing started");
                            },
                            _ => {
                                tracing::debug!(
                                    "LSP status changed: {:?} -> {:?}",
                                    *status_guard,
                                    new_status
                                );
                            },
                        }
                        *status_guard = new_status.clone();
                        drop(status_guard);

                        // Debounce UI notifications to max 5 per second (200ms interval)
                        // Always notify when transitioning to Ready for accurate final state
                        let should_notify = match last_notify {
                            None => true,
                            Some(last) => {
                                last.elapsed() >= Duration::from_millis(PROGRESS_DEBOUNCE_MS)
                            },
                        } || matches!(new_status, LspStatus::Ready);

                        if should_notify {
                            last_notify = Some(Instant::now());
                            let _ = view_clone.update(cx, |_this, cx| {
                                cx.notify();
                            });
                        }
                    }
                }
            })
            .detach();
        }

        // Spawn rust-analyzer
        {
            let lsp_manager_clone = self.lsp_manager.clone();
            let lsp_state_clone = self.lsp_state.clone();
            let view_clone = view.clone();

            cx.spawn(async move |cx| {
                *lsp_state_clone.status.write() = LspStatus::Initializing;
                tracing::debug!("LSP initializing");

                // Notify view
                let _ = view_clone.update(cx, |_this, cx| {
                    cx.notify();
                });

                let rust_analyzer_path = which::which("rust-analyzer")?;

                tracing::info!("Spawning rust-analyzer from: {:?}", rust_analyzer_path);

                let transport = stoat_lsp::StdioTransport::spawn(
                    rust_analyzer_path,
                    vec![],
                    cx.background_executor().clone(),
                )?;

                let server_id = lsp_manager_clone.add_server("rust-analyzer", Arc::new(transport));

                let initialize_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "params": {
                        "processId": std::process::id(),
                        "rootUri": format!("file://{}", std::env::current_dir()?.display()),
                        "capabilities": {
                            "textDocument": {
                                "publishDiagnostics": {
                                    "relatedInformation": true,
                                    "versionSupport": true,
                                }
                            },
                            "window": {
                                "workDoneProgress": true,
                            }
                        },
                    }
                });

                let response = lsp_manager_clone
                    .request(server_id, initialize_request)?
                    .await?;

                tracing::info!("rust-analyzer initialized: {:?}", response);

                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialized",
                    "params": {}
                });

                lsp_manager_clone
                    .notify(server_id, initialized_notification)
                    .await?;

                lsp_manager_clone.start_listener(server_id)?;

                *lsp_state_clone.status.write() = LspStatus::Ready;
                tracing::debug!("LSP ready");

                // Notify view
                let _ = view_clone.update(cx, |_this, cx| {
                    cx.notify();
                });

                Ok::<_, anyhow::Error>(())
            })
            .detach();
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

    /// Change the current working directory.
    ///
    /// Updates the worktree, git repository, and LSP server root to point to the new directory.
    /// Relative paths are resolved against the current worktree root.
    ///
    /// # Arguments
    ///
    /// * `path` - Target directory path (absolute or relative)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if the path doesn't exist or isn't accessible.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Path doesn't exist
    /// - Path is not a directory
    /// - Unable to access the directory
    pub fn change_directory(&mut self, path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        // Resolve relative paths against current worktree root
        let current_root = self.worktree.lock().root().to_path_buf();
        let target_path = if path.is_absolute() {
            path
        } else {
            current_root.join(path)
        };

        // Canonicalize to resolve . and .. and validate existence
        let canonical_path = target_path
            .canonicalize()
            .map_err(|e| format!("Cannot access directory '{}': {}", target_path.display(), e))?;

        // Verify it's a directory
        if !canonical_path.is_dir() {
            return Err(format!("'{}' is not a directory", canonical_path.display()).into());
        }

        // Replace worktree with new root
        *self.worktree.lock() = Worktree::new(canonical_path.clone());

        // Update git repository
        if let Ok(repo) = crate::git::repository::Repository::open(&canonical_path) {
            let branch_info = crate::git::status::gather_git_branch_info(repo.inner());
            let status_files =
                crate::git::status::gather_git_status(repo.inner()).unwrap_or_else(|_| Vec::new());
            let dirty_count = status_files.len();

            self.git_status.branch_info = branch_info;
            self.git_status.files = status_files.clone();
            self.git_status.filtered = status_files;
            self.git_status.dirty_count = dirty_count;
        } else {
            self.git_status.branch_info = None;
            self.git_status.files.clear();
            self.git_status.filtered.clear();
            self.git_status.dirty_count = 0;
        }

        // FIXME: Restart LSP server with new root directory
        // Currently LSP keeps running with the old root. To properly restart:
        // 1. Shutdown active servers via notification/request
        // 2. Spawn new rust-analyzer process
        // 3. Add server via lsp_manager.add_server()
        // 4. Send initialize request with new rootUri
        // 5. Set capabilities and start listener
        // See setup_lsp_progress_tracking for reference implementation

        // Update file finder with new worktree
        self.file_finder = FileFinder::default();

        tracing::info!("Changed directory to: {}", canonical_path.display());

        Ok(())
    }

    /// Get mode for the given KeyContext.
    ///
    /// Returns the appropriate mode string based on the context:
    /// - `TextEditor`: Returns `text_editor_mode` (shared by all text editor panes)
    /// - `CommandPalette`, `CommandPaletteV2`, `InlineInput`: Returns `inline_editor_mode`
    /// - Other contexts: Default to `text_editor_mode`
    pub fn mode_for_context(&self, key_context: KeyContext) -> &str {
        match key_context {
            KeyContext::TextEditor => &self.text_editor_mode,
            KeyContext::CommandPalette | KeyContext::CommandPaletteV2 | KeyContext::InlineInput => {
                &self.inline_editor_mode
            },
            _ => &self.text_editor_mode,
        }
    }

    /// Set mode for the given KeyContext.
    ///
    /// Updates the appropriate mode field based on the context:
    /// - `TextEditor`: Updates `text_editor_mode` (affects all text editor panes)
    /// - `CommandPalette`, `CommandPaletteV2`, `InlineInput`: Updates `inline_editor_mode`
    /// - Other contexts: Updates `text_editor_mode` by default
    pub fn set_mode_for_context(&mut self, key_context: KeyContext, mode: String) {
        match key_context {
            KeyContext::TextEditor => self.text_editor_mode = mode,
            KeyContext::CommandPalette | KeyContext::CommandPaletteV2 | KeyContext::InlineInput => {
                self.inline_editor_mode = mode
            },
            _ => self.text_editor_mode = mode,
        }
    }
}
