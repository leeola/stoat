//! Core Stoat editor entity with Context<Self> pattern.
//!
//! This follows Zed's Buffer architecture - Stoat is an Entity that can spawn
//! self-updating async tasks.

use crate::{
    buffer::{item::BufferItem, store::BufferStore},
    cursor::CursorManager,
    git::{diff::BufferDiff, repository::Repository},
    scroll::ScrollPosition,
    selections::SelectionsCollection,
    worktree::Worktree,
};
use gpui::{App, AppContext, Context, Entity, EventEmitter, WeakEntity};
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
    /// Command palette V2 modal context (uses InlineEditor)
    CommandPaletteV2,
    /// Generic inline input context (for future modal inputs)
    InlineInput,
    /// Diff review mode context
    DiffReview,
    /// Help modal context
    HelpModal,
    /// About modal context
    AboutModal,
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
            Self::CommandPaletteV2 => "CommandPaletteV2",
            Self::InlineInput => "InlineInput",
            Self::DiffReview => "DiffReview",
            Self::HelpModal => "HelpModal",
            Self::AboutModal => "AboutModal",
        }
    }

    /// Parse a KeyContext from string, validating it's a known context.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "TextEditor" => Ok(Self::TextEditor),
            "Git" => Ok(Self::Git),
            "FileFinder" => Ok(Self::FileFinder),
            "BufferFinder" => Ok(Self::BufferFinder),
            "CommandPalette" => Ok(Self::CommandPalette),
            "CommandPaletteV2" => Ok(Self::CommandPaletteV2),
            "InlineInput" => Ok(Self::InlineInput),
            "DiffReview" => Ok(Self::DiffReview),
            "HelpModal" => Ok(Self::HelpModal),
            "AboutModal" => Ok(Self::AboutModal),
            _ => Err(format!("Unknown KeyContext: {s}")),
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
    /// Whether entering this mode should initialize an anchored selection at the cursor.
    ///
    /// When `true`, entering this mode creates an empty selection at the current cursor
    /// position, which serves as an anchor point. Subsequent movement commands will extend
    /// the selection from this anchor. Used for visual selection modes (visual, visual_line,
    /// visual_block).
    pub anchored_selection: bool,
}

impl Mode {
    /// Create a new mode with the given name and display name.
    pub fn new(
        name: impl Into<String>,
        display_name: impl Into<String>,
        anchored_selection: bool,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            previous: None,
            anchored_selection,
        }
    }

    /// Create a new mode with an explicit previous mode override.
    pub fn with_previous(
        name: impl Into<String>,
        display_name: impl Into<String>,
        previous: impl Into<String>,
        anchored_selection: bool,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            previous: Some(previous.into()),
            anchored_selection,
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
    /// Whether this command is hidden from command palette by default
    pub hidden: bool,
}

/// Events emitted by Stoat
#[derive(Clone, Debug)]
pub enum StoatEvent {
    /// Editor content or state changed
    Changed,
    /// Pane-level action dispatched from the compiled keymap
    Action { name: String, args: Vec<String> },
    /// A file was opened (used to trigger deferred LSP startup)
    FileOpened { language: Language },
}

/// Main editor entity.
///
/// Key difference from old stoat: methods take `&mut Context<Self>` instead of `&mut App`.
/// This enables spawning self-updating async tasks.
pub struct Stoat {
    /// Global configuration loaded from config.toml
    ///
    /// Initialized at startup by loading from the platform-specific config directory.
    /// Used to configure editor behavior like fonts, themes, and keybindings.
    pub(crate) config: crate::config::Config,

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

    /// Initial buffer ID (for auto-drop when empty)
    pub(crate) initial_buffer_id: Option<BufferId>,

    /// Multi-cursor selection management using Zed's architecture
    ///
    /// Stores selections as [`Selection<Anchor>`] for persistence across buffer edits.
    /// Uses [`Arc`] for cheap cloning and lazy anchor resolution for performance.
    /// See [`SelectionsCollection`] for details.
    pub(crate) selections: SelectionsCollection,

    /// Legacy single-cursor manager (for backward compatibility during migration)
    ///
    /// This field is maintained alongside selections to avoid breaking existing code.
    /// New code should use the selections field and related methods for multi-cursor support.
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

    /// Temporary reference to file_finder input buffer (set when entering FileFinder context)
    ///
    /// This is a reference (not the owner) to the file_finder input buffer from AppState.
    /// It allows edit actions (insert_text, delete_left) to route to the file finder input
    /// while maintaining the architectural separation where file_finder state lives in app state.
    pub(crate) file_finder_input_ref: Option<Entity<Buffer>>,

    /// Temporary reference to command_palette input buffer (set when entering CommandPalette
    /// context)
    ///
    /// This is a reference (not the owner) to the command_palette input buffer from
    /// AppState. It allows edit actions (insert_text, delete_left) to route to the
    /// command palette input while maintaining the architectural separation where
    /// command_palette state lives in app state.
    pub(crate) command_palette_input_ref: Option<Entity<Buffer>>,

    /// Temporary reference to buffer_finder input buffer (set when entering BufferFinder context)
    ///
    /// Similar to [`command_palette_input_ref`], this is a reference (not the owner) to
    /// the buffer_finder input buffer from AppState. It allows edit actions to route
    /// to the buffer finder input while maintaining architectural separation.
    pub(crate) buffer_finder_input_ref: Option<Entity<Buffer>>,

    // Help modal state
    pub(crate) help_modal_previous_mode: Option<String>,
    pub(crate) help_modal_previous_key_context: Option<KeyContext>,

    // About modal state
    pub(crate) about_modal_previous_mode: Option<String>,
    pub(crate) about_modal_previous_key_context: Option<KeyContext>,

    // Diff review state
    /// List of modified files for diff review.
    ///
    /// Populated when entering diff review mode. Diffs are computed on-demand when
    /// loading each file, using
    /// [`compute_diff_for_review_mode`](Self::compute_diff_for_review_mode).
    pub(crate) diff_review_files: Vec<PathBuf>,
    pub(crate) diff_review_current_file_idx: usize,
    pub(crate) diff_review_current_hunk_idx: usize,
    pub(crate) diff_review_approved_hunks:
        std::collections::HashMap<PathBuf, std::collections::HashSet<usize>>,
    pub(crate) diff_review_previous_mode: Option<String>,
    /// Comparison mode for diff review (working vs HEAD, working vs index, or index vs HEAD).
    ///
    /// Determines which git refs are compared when computing diffs in review mode.
    /// Default is [`WorkingVsHead`](crate::git::diff_review::DiffComparisonMode::WorkingVsHead).
    pub(crate) diff_review_comparison_mode: crate::git::diff_review::DiffComparisonMode,

    /// Line-level selection within a hunk for partial staging.
    pub(crate) line_selection: Option<crate::git::line_selection::LineSelection>,

    /// Current file path (for status bar display)
    pub(crate) current_file_path: Option<PathBuf>,

    /// LSP document version numbers per file
    pub(crate) buffer_versions: HashMap<PathBuf, i32>,

    /// LSP manager for language server communication
    pub(crate) lsp_manager: Option<Arc<stoat_lsp::LspManager>>,

    /// Worktree for file scanning
    pub(crate) worktree: Arc<Mutex<Worktree>>,

    /// Parent stoat when this is a minimap instance.
    ///
    /// When `Some`, this Stoat is acting as a minimap for the parent editor.
    /// Following Zed's pattern, the minimap is just another Stoat instance with
    /// tiny font and this parent reference to synchronize scroll and handle interactions.
    pub(crate) parent_stoat: Option<WeakEntity<Stoat>>,

    /// DisplayMap for coordinate transformations (wrapping, folding, inlays, blocks).
    ///
    /// Transforms buffer coordinates to display coordinates by applying layers:
    /// - InlayMap: Adds inline type hints and parameter names
    /// - FoldMap: Hides folded code regions
    /// - TabMap: Expands tabs to spaces
    /// - WrapMap: Soft wraps long lines
    /// - BlockMap: Inserts custom visual blocks
    ///
    /// Minimaps share the parent's DisplayMap via cheap Entity clones.
    pub(crate) display_map: Entity<stoat_text_transform::DisplayMap>,

    /// State for SelectNext/SelectAllMatches occurrence selection.
    ///
    /// Tracks the search query and iteration state for occurrence-based multi-cursor
    /// selection. Created on first invocation of [`select_next`](Self::select_next) and
    /// reused for subsequent invocations with the same query.
    pub(crate) select_next_state: Option<crate::editor::state::SelectNextState>,

    /// State for SelectPrevious occurrence selection.
    ///
    /// Tracks the search query and iteration state for backward occurrence-based
    /// multi-cursor selection. Created on first invocation of
    /// [`select_previous`](Self::select_previous) and reused for subsequent
    /// invocations with the same query.
    pub(crate) select_prev_state: Option<crate::editor::state::SelectNextState>,

    /// Compiled keymap from stcfg configuration, shared across all views.
    pub(crate) compiled_keymap: Arc<crate::keymap::compiled::CompiledKeymap>,
}

impl EventEmitter<StoatEvent> for Stoat {}

impl crate::keymap::compiled::KeymapState for Stoat {
    fn get_string(&self, name: &str) -> Option<&str> {
        match name {
            "focus" => Some(self.key_context.as_str()),
            "mode" => Some(&self.mode),
            _ => None,
        }
    }

    fn get_number(&self, _name: &str) -> Option<f64> {
        None
    }

    fn get_bool(&self, _name: &str) -> Option<bool> {
        None
    }
}

impl Stoat {
    /// Create new Stoat entity.
    ///
    /// Takes `&mut Context<Self>` to follow Zed's Buffer pattern.
    ///
    /// # Arguments
    ///
    /// * `config` - Global configuration loaded from config.toml
    /// * `worktree` - Shared worktree from workspace
    /// * `buffer_store` - Shared buffer store from workspace
    /// * `lsp_manager` - Optional LSP manager for language server communication
    /// * `cx` - GPUI context for entity creation
    pub fn new(
        config: crate::config::Config,
        worktree: Arc<Mutex<Worktree>>,
        buffer_store: Entity<BufferStore>,
        lsp_manager: Option<Arc<stoat_lsp::LspManager>>,
        compiled_keymap: Arc<crate::keymap::compiled::CompiledKeymap>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_text(
            config,
            worktree,
            buffer_store,
            lsp_manager,
            compiled_keymap,
            "",
            cx,
        )
    }

    /// Create new Stoat with specific initial buffer text (primarily for tests).
    pub fn new_with_text(
        config: crate::config::Config,
        worktree: Arc<Mutex<Worktree>>,
        buffer_store: Entity<BufferStore>,
        lsp_manager: Option<Arc<stoat_lsp::LspManager>>,
        compiled_keymap: Arc<crate::keymap::compiled::CompiledKeymap>,
        initial_text: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        // Use workspace's buffer store (shared across all editors)

        // Allocate buffer ID from BufferStore to prevent collisions
        let buffer_id = buffer_store.update(cx, |store, _cx| store.allocate_buffer_id());

        // Create initial buffer with specified text
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, initial_text));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer.clone(), Language::PlainText, cx));

        // Register buffer in BufferStore (weak ref) and store strong ref in open_buffers
        buffer_store.update(cx, |store, _cx| {
            store.register_buffer(buffer_id, &buffer_item, None);
        });
        let open_buffers = vec![buffer_item.clone()];
        let active_buffer_id = Some(buffer_id);
        let initial_buffer_id = Some(buffer_id);

        // Initialize cursor and selections at origin
        let cursor = CursorManager::new();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let selections = SelectionsCollection::new(&buffer_snapshot);

        let modes = crate::keymap::default_modes();
        let contexts = crate::keymap::default_contexts();
        let key_context = KeyContext::TextEditor; // Default context

        // Initialize DisplayMap for coordinate transformations
        let display_map = {
            let tab_width = 4; // Default tab width
            let font = gpui::Font {
                family: gpui::SharedString::from(config.buffer_font_family.clone()),
                features: Default::default(),
                weight: gpui::FontWeight::NORMAL,
                style: gpui::FontStyle::Normal,
                fallbacks: None,
            };
            let font_size = gpui::px(config.buffer_font_size);
            let wrap_width = None; // Will be set dynamically based on viewport

            cx.new(|cx| {
                stoat_text_transform::DisplayMap::new(
                    buffer.clone(),
                    tab_width,
                    font,
                    font_size,
                    wrap_width,
                    cx,
                )
            })
        };

        Self {
            config,
            buffer_store,
            open_buffers,
            active_buffer_id,
            initial_buffer_id,
            selections,
            cursor,
            scroll: ScrollPosition::new(),
            viewport_lines: None,
            mode: "normal".into(),
            modes,
            key_context,
            contexts,
            file_finder_input_ref: None,
            command_palette_input_ref: None,
            buffer_finder_input_ref: None,
            help_modal_previous_mode: None,
            help_modal_previous_key_context: None,
            about_modal_previous_mode: None,
            about_modal_previous_key_context: None,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            diff_review_comparison_mode: crate::git::diff_review::DiffComparisonMode::default(),
            line_selection: None,
            current_file_path: None,
            buffer_versions: HashMap::new(),
            lsp_manager,
            worktree,
            parent_stoat: None,
            display_map,
            select_next_state: None,
            select_prev_state: None,
            compiled_keymap,
        }
    }

    /// Get the global configuration.
    ///
    /// Returns a reference to the [`Config`](crate::config::Config) loaded at application startup.
    /// Used by the GUI layer to access font settings and other user preferences.
    pub fn config(&self) -> &crate::config::Config {
        &self.config
    }

    /// Access the DisplayMap for coordinate transformations.
    ///
    /// Minimap instances delegate to their parent's DisplayMap.
    /// Returns the DisplayMap entity for converting between buffer and display coordinates.
    pub fn display_map(&self, cx: &App) -> Entity<stoat_text_transform::DisplayMap> {
        // If this is a minimap, delegate to parent
        if let Some(parent_weak) = &self.parent_stoat {
            if let Some(parent) = parent_weak.upgrade() {
                return parent.read(cx).display_map(cx);
            }
        }

        self.display_map.clone()
    }

    /// Clone this Stoat for a split pane.
    ///
    /// Creates a new Stoat instance that shares the same [`BufferItem`] (text buffer)
    /// but has independent cursor position, scroll position, and viewport state.
    /// This follows Zed's performant pattern: share expensive buffer data via [`Entity`],
    /// but maintain independent view state per pane.
    ///
    /// The selections and scroll positions are copied to the new instance, so the split starts
    /// at the same view, but subsequent movements are independent.
    ///
    /// Used by [`PaneGroupView`] when splitting panes to create multiple views of the same buffer.
    pub fn clone_for_split(&self) -> Self {
        Self {
            config: self.config.clone(),
            buffer_store: self.buffer_store.clone(),
            open_buffers: self.open_buffers.clone(),
            active_buffer_id: self.active_buffer_id,
            initial_buffer_id: None,
            selections: self.selections.clone(),
            cursor: self.cursor.clone(),
            scroll: self.scroll.clone(),
            viewport_lines: self.viewport_lines,
            mode: self.mode.clone(),
            modes: self.modes.clone(),
            key_context: self.key_context,
            contexts: self.contexts.clone(),
            file_finder_input_ref: None,
            command_palette_input_ref: None,
            buffer_finder_input_ref: None,
            help_modal_previous_mode: None,
            help_modal_previous_key_context: None,
            about_modal_previous_mode: None,
            about_modal_previous_key_context: None,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            diff_review_comparison_mode: self.diff_review_comparison_mode,
            line_selection: None,
            current_file_path: self.current_file_path.clone(),
            buffer_versions: self.buffer_versions.clone(),
            lsp_manager: self.lsp_manager.clone(),
            worktree: self.worktree.clone(),
            parent_stoat: None,
            display_map: self.display_map.clone(),
            select_next_state: None,
            select_prev_state: None,
            compiled_keymap: self.compiled_keymap.clone(),
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

    /// Switch to a different buffer by ID.
    ///
    /// Updates the active buffer ID, current file path, activation history, and resets
    /// cursor/selections to the beginning of the buffer. Used by buffer finder to switch
    /// between open buffers.
    ///
    /// # Arguments
    ///
    /// * `buffer_id` - The buffer ID to switch to
    /// * `cx` - GPUI context
    ///
    /// # Returns
    ///
    /// `Ok(())` if the switch succeeded, `Err` if the buffer was not found in BufferStore.
    pub fn switch_to_buffer(
        &mut self,
        buffer_id: BufferId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // Get buffer from BufferStore
        let buffer_item = self
            .buffer_store
            .read(cx)
            .get_buffer(buffer_id)
            .ok_or_else(|| format!("Buffer not found: {buffer_id:?}"))?;

        // Update active_buffer_id
        self.active_buffer_id = Some(buffer_id);

        // Update current_file_path for status bar
        self.current_file_path = self
            .buffer_store
            .read(cx)
            .get_path(buffer_id)
            .map(|p| self.normalize_file_path(p));

        // Update activation history
        self.buffer_store.update(cx, |store, _cx| {
            store.activate_buffer(buffer_id);
        });

        // Reset cursor to beginning
        let target_pos = text::Point::new(0, 0);
        self.cursor.move_to(target_pos);

        // Sync selections to cursor position
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: target_pos,
                end: target_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &buffer_snapshot,
        );

        cx.notify();
        Ok(())
    }

    /// Get all active selections resolved to Point positions.
    ///
    /// Returns all non-overlapping selections with anchors resolved to concrete
    /// Point positions using the active buffer's snapshot. This is the primary
    /// way to access selections for most operations.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let selections: Vec<Selection<Point>> = stoat.active_selections(cx);
    /// for sel in selections {
    ///     println!("Selection from {:?} to {:?}", sel.start, sel.end);
    /// }
    /// ```
    pub fn active_selections(&self, cx: &App) -> Vec<text::Selection<Point>> {
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        self.selections.all(&buffer_snapshot)
    }

    /// Get the newest (primary) selection resolved to Point.
    ///
    /// Returns the most recently created selection, which is typically the one
    /// the user interacts with when only one cursor is active.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let selection = stoat.newest_selection(cx);
    /// let cursor_pos = selection.head();
    /// ```
    pub fn newest_selection(&self, cx: &App) -> text::Selection<Point> {
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        self.selections.newest(&buffer_snapshot)
    }

    /// Get cursor position (legacy single-cursor API).
    ///
    /// Returns the position from the cursor field for backward compatibility.
    /// For multi-cursor support, use [`newest_selection`](Self::newest_selection).
    pub fn cursor_position(&self) -> Point {
        self.cursor.position()
    }

    /// Set cursor position (legacy single-cursor API).
    ///
    /// Updates the cursor field for backward compatibility.
    /// For multi-cursor support, use [`SelectionsCollection::select`].
    pub fn set_cursor_position(&mut self, position: Point) {
        self.cursor.move_to(position);
    }

    /// Get current selection (legacy single-cursor API).
    ///
    /// Returns the selection from the cursor field for backward compatibility.
    /// For multi-cursor support, use [`active_selections`](Self::active_selections).
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

    /// Sync mode field with AppState's mode for current KeyContext.
    ///
    /// Call this after changing key_context to ensure mode field reflects
    /// the appropriate shared mode state from AppState.
    pub fn sync_mode_to_context(&mut self, app_state: &crate::app_state::AppState) {
        let new_mode = app_state.mode_for_context(self.key_context);
        self.mode = new_mode.to_string();
    }

    /// Get mode metadata by name.
    ///
    /// Returns the [`Mode`] struct containing display name and other metadata
    /// for the given mode name, or `None` if the mode is not registered.
    pub fn get_mode(&self, name: &str) -> Option<&Mode> {
        self.modes.get(name)
    }

    /// Check if the current mode has anchored selection behavior.
    ///
    /// Returns `true` if the current mode's metadata has `anchored_selection` set to `true`.
    /// This is used by movement and selection actions to determine whether to extend
    /// selections (in anchored mode) or collapse them (in non-anchored mode).
    ///
    /// Modes with anchored selections include visual, visual_line, and visual_block.
    pub fn is_mode_anchored(&self) -> bool {
        self.get_mode(&self.mode)
            .map(|m| m.anchored_selection)
            .unwrap_or(false)
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
    /// exiting review mode (for position restoration), we check both mode and files list.
    /// Used by GUI to adjust gutter width and show diff backgrounds.
    pub fn is_in_diff_review(&self, cx: &App) -> bool {
        if let Some(parent) = &self.parent_stoat {
            if let Some(parent) = parent.upgrade() {
                return parent.read(cx).is_in_diff_review(cx);
            }
        }
        (self.mode == "diff_review" || self.mode == "line_select")
            && !self.diff_review_files.is_empty()
    }

    /// Get diff review progress as (reviewed_count, total_count).
    ///
    /// Returns [`None`] since we no longer pre-compute all hunks. With on-demand loading,
    /// we can't know the total number of hunks without loading all files.
    ///
    /// Future: Could scan files to count hunks without computing full diffs, or show
    /// file-level progress instead.
    pub fn diff_review_progress(&self) -> Option<(usize, usize)> {
        None
    }

    /// Get current file progress in review as (current_file, total_files).
    ///
    /// Returns [`None`] if not in review mode. Used by status bar to show progress like "File 2/5".
    ///
    /// # Returns
    ///
    /// `Some((current, total))` where both are 1-indexed for display
    pub fn diff_review_file_progress(&self, cx: &App) -> Option<(usize, usize)> {
        if !self.is_in_diff_review(cx) {
            return None;
        }

        Some((
            self.diff_review_current_file_idx + 1, // 1-indexed for display
            self.diff_review_files.len(),
        ))
    }

    /// Get current hunk position across all files in diff review.
    ///
    /// Computes the global hunk position by scanning all files in the current comparison
    /// mode and counting hunks. Used by status bar to show "Patch X/Y" progress indicator.
    ///
    /// # Workflow
    ///
    /// 1. Discovers git repository from worktree root
    /// 2. For each file: reads content from disk and git, counts hunks directly
    /// 3. Adds current hunk index + 1 to get current position
    /// 4. Continues scanning remaining files to get total count
    ///
    /// # Returns
    ///
    /// `Some((current, total))` where both are 1-indexed for display, or [`None`] if not in
    /// diff review mode or if git operations fail.
    ///
    /// # Performance
    ///
    /// This method computes hunk counts for all files on every call to get accurate counts.
    /// Uses [`crate::git::diff::count_hunks`] which is fast (no buffer allocations).
    /// Called from GUI status bar rendering, so should be fast enough for typical usage.
    /// Consider caching if performance becomes an issue.
    ///
    /// # Related
    ///
    /// - [`crate::git::diff::count_hunks`] - fast hunk counting from text
    /// - [`diff_comparison_mode`](Self::diff_comparison_mode) - determines which refs to compare
    pub fn diff_review_hunk_position(&self, cx: &App) -> Option<(usize, usize)> {
        if !self.is_in_diff_review(cx) {
            return None;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(e) => {
                tracing::error!(
                    "diff_review_hunk_position: failed to discover repository at {:?}: {}",
                    root_path,
                    e
                );
                return None;
            },
        };

        // Use git2's diff API to count hunks efficiently
        let hunk_counts = match repo.count_hunks_by_file(self.diff_review_comparison_mode) {
            Ok(counts) => counts,
            Err(e) => {
                tracing::error!("diff_review_hunk_position: failed to count hunks: {}", e);
                return None;
            },
        };

        let mut total_hunks = 0;
        let mut hunks_before_current = 0;

        for (file_idx, file_path) in self.diff_review_files.iter().enumerate() {
            // Look up hunk count for this file (0 if not in the map)
            let hunk_count = hunk_counts.get(file_path).copied().unwrap_or(0);

            if file_idx < self.diff_review_current_file_idx {
                hunks_before_current += hunk_count;
            }

            total_hunks += hunk_count;
        }

        if total_hunks == 0 {
            tracing::debug!(
                "diff_review_hunk_position: no hunks found across {} files in {:?} mode",
                self.diff_review_files.len(),
                self.diff_review_comparison_mode
            );
            return Some((0, 0));
        }

        let current_position = hunks_before_current + self.diff_review_current_hunk_idx + 1;

        tracing::debug!(
            "diff_review_hunk_position: position {}/{} (current_file={}, current_hunk={}, mode={:?})",
            current_position,
            total_hunks,
            self.diff_review_current_file_idx,
            self.diff_review_current_hunk_idx,
            self.diff_review_comparison_mode
        );

        Some((current_position, total_hunks))
    }

    /// Get the parent stoat if this is a minimap.
    ///
    /// Returns the parent editor entity if this is a minimap, or `None` for regular editors.
    pub fn parent_stoat(&self) -> Option<&WeakEntity<Stoat>> {
        self.parent_stoat.as_ref()
    }

    /// Get the current diff comparison mode.
    ///
    /// Returns the active comparison mode used in diff review for determining which
    /// git refs are compared (working vs HEAD, working vs index, or index vs HEAD).
    ///
    /// # Returns
    ///
    /// The current [`DiffComparisonMode`](crate::git::diff_review::DiffComparisonMode)
    pub fn diff_comparison_mode(&self) -> crate::git::diff_review::DiffComparisonMode {
        self.diff_review_comparison_mode
    }

    /// Set the diff comparison mode.
    ///
    /// Changes which git refs are compared in diff review mode. This affects which hunks
    /// are visible when reviewing files.
    ///
    /// # Arguments
    ///
    /// * `mode` - The new comparison mode to use
    ///
    /// # Usage
    ///
    /// This method can be called from an action to switch between reviewing all changes,
    /// only unstaged changes, or only staged changes. Future PRs will add keybindings
    /// to cycle through modes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Switch to viewing only unstaged changes
    /// stoat.set_diff_comparison_mode(DiffComparisonMode::WorkingVsIndex);
    /// ```
    pub fn set_diff_comparison_mode(&mut self, mode: crate::git::diff_review::DiffComparisonMode) {
        self.diff_review_comparison_mode = mode;
    }

    /// Cycle to the next diff comparison mode.
    ///
    /// Rotates through comparison modes in order: WorkingVsHead -> WorkingVsIndex ->
    /// IndexVsHead -> WorkingVsHead. This is a convenience method for toggling between
    /// modes via a single keybinding.
    ///
    /// # Usage
    ///
    /// This method is designed to be called from an action bound to a key. Future PRs
    /// will add the action and keybinding.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // User presses keybinding to cycle comparison mode
    /// stoat.cycle_diff_comparison_mode();
    /// ```
    pub fn cycle_diff_comparison_mode(&mut self) {
        self.diff_review_comparison_mode = self.diff_review_comparison_mode.next();
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
    ///
    /// Scrolls the viewport to ensure the cursor is visible with padding above and below.
    pub fn ensure_cursor_visible(&mut self, cx: &mut App) {
        let Some(viewport_lines) = self.viewport_lines else {
            return;
        };

        // Convert cursor buffer position to display position (handles wrapping, folding, etc.)
        let cursor_buffer_pos = self.cursor.position();
        let display_snapshot = self.display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let cursor_display_point =
            display_snapshot.point_to_display_point(cursor_buffer_pos, sum_tree::Bias::Left);
        let cursor_display_row = cursor_display_point.row as f32;

        let scroll_y = self.scroll.position.y;
        let last_visible_line = scroll_y + viewport_lines;

        const PADDING: f32 = 3.0;

        if cursor_display_row < scroll_y + PADDING {
            // Scrolling up: position cursor PADDING lines from top
            let target_scroll_y = (cursor_display_row - PADDING).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        } else if cursor_display_row >= last_visible_line - PADDING {
            // Scrolling down: position cursor PADDING lines from bottom
            let target_scroll_y = (cursor_display_row - viewport_lines + PADDING + 1.0).max(0.0);
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }
    }

    /// Normalize a file path for display.
    ///
    /// Converts any path (absolute or relative) to a clean relative path:
    /// - Strips worktree root prefix from absolute paths
    /// - Removes leading `./` prefix if present
    ///
    /// This is the single source of truth for path normalization in the editor.
    /// All code that stores [`current_file_path`](Self::current_file_path) should use this.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to normalize (absolute or relative)
    ///
    /// # Returns
    ///
    /// Normalized path relative to worktree root without `./` prefix
    pub(crate) fn worktree_root_abs(&self) -> PathBuf {
        let root = self.worktree.lock().root().to_path_buf();
        root.canonicalize().unwrap_or(root)
    }

    pub(crate) fn normalize_file_path(&self, path: &std::path::Path) -> PathBuf {
        let root = self.worktree.lock().root().to_path_buf();

        // Canonicalize root to handle relative paths like "."
        // This converts "." to absolute path (e.g., /Users/lee/projects/stoat)
        // so strip_prefix works correctly with absolute paths from diff review
        let absolute_root = root.canonicalize().unwrap_or(root);

        let relative = path.strip_prefix(&absolute_root).unwrap_or(path);

        // Strip leading "./" if present
        let path_str = relative.display().to_string();
        if let Some(cleaned) = path_str.strip_prefix("./") {
            PathBuf::from(cleaned)
        } else {
            relative.to_path_buf()
        }
    }

    fn increment_buffer_version(&mut self, path: &std::path::Path) -> i32 {
        let version = self.buffer_versions.entry(path.to_path_buf()).or_insert(0);
        *version += 1;
        *version
    }

    pub(crate) fn send_did_change_notification(&mut self, cx: &mut Context<Self>) {
        // Check and clone what we need first to avoid borrow conflicts
        let (lsp_manager, path) = match (&self.lsp_manager, &self.current_file_path) {
            (Some(m), Some(p)) => (m.clone(), p.clone()),
            _ => return,
        };

        // Now we can mutably borrow self
        let version = self.increment_buffer_version(&path);
        let buffer = self.active_buffer(cx).read(cx).buffer();
        let text = buffer.read(cx).text();

        let uri_str = format!("file://{}", path.display());
        if let Ok(uri) = uri_str.parse::<lsp_types::Uri>() {
            let executor = cx.background_executor().clone();
            for server_id in lsp_manager.active_servers() {
                let lsp_manager = lsp_manager.clone();
                let uri = uri.clone();
                let text = text.clone();

                executor
                    .spawn(async move {
                        if let Err(e) = lsp_manager.did_change(server_id, uri, version, text).await
                        {
                            tracing::warn!("Failed to send didChange notification: {}", e);
                        }
                    })
                    .detach();
            }
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

        // Get mtime after reading file
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

        // Detect line ending from file contents
        let line_ending = text::LineEnding::detect(&contents);

        // Update the buffer content
        buffer_item_entity.update(cx, |item, cx| {
            item.buffer().update(cx, |buffer, _| {
                let len = buffer.len();
                buffer.edit([(0..len, contents.as_str())]);
            });
            let _ = item.reparse(cx);
            // Set saved text baseline, mtime, and line ending for modification tracking
            item.set_saved_text(contents.clone());
            if let Some(mtime) = mtime {
                item.set_saved_mtime(mtime);
            }
            item.set_line_ending(line_ending);
        });

        // Store strong reference in open_buffers if not already present
        if !self
            .open_buffers
            .iter()
            .any(|item| item.read(cx).buffer().read(cx).remote_id() == buffer_id)
        {
            self.open_buffers.push(buffer_item_entity.clone());
        }

        // Compute git diff and staged row ranges
        buffer_item_entity.update(cx, |item, cx| {
            if let Ok(repo) = Repository::discover(path) {
                if let Ok(head_content) = repo.head_content(path) {
                    let buffer_snapshot = item.buffer().read(cx).snapshot();
                    let buffer_id = buffer_snapshot.remote_id();
                    match BufferDiff::new(buffer_id, head_content.clone(), &buffer_snapshot) {
                        Ok(ref diff) => {
                            if let Ok(index_content) = repo.index_content(path) {
                                let wi_diff =
                                    BufferDiff::new(buffer_id, index_content, &buffer_snapshot)
                                        .ok();
                                let ranges = crate::git::diff::compute_staged_buffer_rows(
                                    diff,
                                    wi_diff.as_ref(),
                                    &buffer_snapshot,
                                );
                                item.set_staged_rows(if ranges.is_empty() {
                                    None
                                } else {
                                    Some(ranges)
                                });
                            }
                            item.set_diff(Some(diff.clone()));
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

        // Update current file path for status bar (normalized)
        self.current_file_path = Some(self.normalize_file_path(&path_buf));

        // Recreate DisplayMap with new buffer to ensure proper subscription
        let buffer = buffer_item_entity.read(cx).buffer().clone();
        self.display_map = {
            let tab_width = 4;
            let font = self.display_map.read(cx).font().clone();
            let font_size = self.display_map.read(cx).font_size();
            let wrap_width = self.display_map.read(cx).wrap_width();
            cx.new(|cx| {
                stoat_text_transform::DisplayMap::new(
                    buffer, tab_width, font, font_size, wrap_width, cx,
                )
            })
        };

        // Reset cursor and selections to origin
        self.cursor.move_to(text::Point::new(0, 0));

        // Initialize new selections for the new buffer
        let new_snapshot = buffer_item_entity.read(cx).buffer().read(cx).snapshot();
        self.selections = SelectionsCollection::new(&new_snapshot);

        cx.notify();

        // Drop initial buffer if it's still empty
        self.maybe_drop_initial_buffer(cx);

        // Send didOpen notification to LSP servers
        let version = self.increment_buffer_version(&path_buf);
        if let Some(lsp_manager) = &self.lsp_manager {
            let uri_str = format!("file://{}", path_buf.display());
            if let Ok(uri) = uri_str.parse::<lsp_types::Uri>() {
                let language_id = match language {
                    Language::Rust => "rust",
                    _ => "plaintext",
                }
                .to_string();

                let executor = cx.background_executor().clone();
                for server_id in lsp_manager.active_servers() {
                    let lsp_manager = lsp_manager.clone();
                    let uri = uri.clone();
                    let language_id = language_id.clone();
                    let contents = contents.clone();

                    executor
                        .spawn(async move {
                            if let Err(e) = lsp_manager
                                .did_open(server_id, uri, language_id, version, contents)
                                .await
                            {
                                tracing::warn!("Failed to send didOpen notification: {}", e);
                            }
                        })
                        .detach();
                }
            }
        }

        cx.emit(StoatEvent::FileOpened { language });

        Ok(())
    }

    /// Drop the initial buffer if it's still empty.
    ///
    /// Called when opening a new buffer to automatically clean up the empty initial buffer
    /// that was created when stoat started. If the user has typed any text in the initial
    /// buffer, it will be kept.
    ///
    /// This method:
    /// - Returns early if no initial buffer is tracked
    /// - Returns early if the initial buffer is currently active (safety check)
    /// - Checks if the initial buffer is empty
    /// - If empty: removes from open_buffers and closes in buffer_store
    /// - Always clears initial_buffer_id after checking (only check once)
    fn maybe_drop_initial_buffer(&mut self, cx: &mut Context<Self>) {
        // If we've already processed or no initial buffer, return early
        let Some(init_id) = self.initial_buffer_id else {
            return;
        };

        // Don't process if initial buffer is the active one (safety check)
        if Some(init_id) == self.active_buffer_id {
            tracing::debug!("Initial buffer {:?} is active, not dropping", init_id);
            return;
        }

        // Get the initial buffer
        let Some(buffer_item) = self.buffer_store.read(cx).get_buffer(init_id) else {
            // Buffer already gone, clear the tracking
            tracing::debug!("Initial buffer {:?} already dropped", init_id);
            self.initial_buffer_id = None;
            return;
        };

        // Check if it's empty
        let is_empty = buffer_item.read(cx).buffer().read(cx).text().is_empty();

        if is_empty {
            // Drop the initial buffer
            tracing::debug!("Dropping empty initial buffer {:?}", init_id);

            // Remove from open_buffers
            self.open_buffers
                .retain(|b| b.read(cx).buffer().read(cx).remote_id() != init_id);

            // Close in buffer store
            self.buffer_store.update(cx, |store, _cx| {
                store.close_buffer(init_id);
            });
        } else {
            tracing::debug!("Initial buffer {:?} has content, keeping it", init_id);
        }

        // Clear initial_buffer_id so we don't check again
        self.initial_buffer_id = None;
    }

    /// Compute diff for the currently active buffer respecting the diff review comparison mode.
    ///
    /// Uses [`diff_review_comparison_mode`](Self::diff_review_comparison_mode) to determine
    /// which git refs to compare. This method is used by diff review to compute diffs based
    /// on whether we're reviewing all changes, only unstaged, or only staged changes.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to compute diff for
    /// * `cx` - GPUI context
    ///
    /// # Returns
    ///
    /// [`Some(BufferDiff)`] if diff computation succeeds, [`None`] if git operations fail
    /// or if the file is not in a git repository.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // After loading a file, recompute its diff for review mode
    /// if let Some(diff) = self.compute_diff_for_review_mode(&file_path, cx) {
    ///     buffer_item.update(cx, |item, _| item.set_diff(Some(diff)));
    /// }
    /// ```
    #[allow(clippy::single_range_in_vec_init)]
    pub(crate) fn compute_diff_for_review_mode(
        &self,
        path: &std::path::Path,
        cx: &App,
    ) -> Option<(BufferDiff, Option<Vec<std::ops::Range<u32>>>)> {
        use crate::git::diff_review::DiffComparisonMode;

        let repo = Repository::discover(path).ok()?;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let buffer_id = buffer_snapshot.remote_id();

        tracing::debug!(
            "compute_diff_for_review_mode: mode={:?}, path={:?}",
            self.diff_review_comparison_mode,
            path
        );

        let (diff, staged_rows) = match self.diff_review_comparison_mode {
            DiffComparisonMode::WorkingVsHead => {
                let base_content = repo.head_content(path).ok()?;
                tracing::debug!(
                    "WorkingVsHead: base_len={}, working_len={}",
                    base_content.len(),
                    buffer_snapshot.text().len()
                );
                let diff =
                    BufferDiff::new(buffer_id, base_content.clone(), &buffer_snapshot).ok()?;
                let staged = repo.index_content(path).ok().map(|index_content| {
                    let wi_diff = BufferDiff::new(buffer_id, index_content, &buffer_snapshot).ok();
                    crate::git::diff::compute_staged_buffer_rows(
                        &diff,
                        wi_diff.as_ref(),
                        &buffer_snapshot,
                    )
                });
                let staged = staged.and_then(|v| if v.is_empty() { None } else { Some(v) });
                (diff, staged)
            },
            DiffComparisonMode::WorkingVsIndex => {
                let base_content = repo.index_content(path).unwrap_or_else(|_| String::new());
                tracing::debug!(
                    "WorkingVsIndex: base_len={}, working_len={}",
                    base_content.len(),
                    buffer_snapshot.text().len()
                );
                let diff = BufferDiff::new(buffer_id, base_content, &buffer_snapshot).ok()?;
                (diff, None)
            },
            DiffComparisonMode::IndexVsHead => {
                let head_content = repo.head_content(path).ok()?;
                tracing::debug!(
                    "IndexVsHead: head_len={}, buffer_len={}",
                    head_content.len(),
                    buffer_snapshot.text().len()
                );
                let diff = BufferDiff::new(buffer_id, head_content, &buffer_snapshot).ok()?;
                (diff, Some(vec![0..u32::MAX]))
            },
        };

        tracing::debug!("Computed diff with {} hunks", diff.hunks.len());
        Some((diff, staged_rows))
    }

    /// Recompute the diff display for the current file and push results into
    /// the active [`BufferItem`]. Called on window re-activation so that
    /// external git operations (e.g. `git add` in a terminal) are reflected.
    pub(crate) fn refresh_git_diff(&mut self, cx: &mut Context<Self>) {
        let Some(file_path) = self.current_file_path.clone() else {
            return;
        };
        if let Some((new_diff, staged_rows)) = self.compute_diff_for_review_mode(&file_path, cx) {
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _| {
                item.set_diff(Some(new_diff));
                item.set_staged_rows(staged_rows);
            });
        }
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
            config: self.config.clone(),
            buffer_store: self.buffer_store.clone(),
            open_buffers: self.open_buffers.clone(),
            active_buffer_id: self.active_buffer_id,
            initial_buffer_id: None,
            selections: self.selections.clone(), // Clone selections for minimap
            cursor: CursorManager::new(),        // New cursor for minimap
            scroll: self.scroll.clone(),         // Clone scroll state (will be synced)
            viewport_lines: None,                // Will be set by layout
            mode: "minimap".into(),              // Special mode for minimap
            modes: self.modes.clone(),
            key_context: KeyContext::TextEditor, // Minimap always in editor context
            contexts: self.contexts.clone(),
            file_finder_input_ref: None,
            command_palette_input_ref: None,
            buffer_finder_input_ref: None,
            help_modal_previous_mode: None,
            help_modal_previous_key_context: None,
            about_modal_previous_mode: None,
            about_modal_previous_key_context: None,
            diff_review_files: Vec::new(),
            diff_review_current_file_idx: 0,
            diff_review_current_hunk_idx: 0,
            diff_review_approved_hunks: std::collections::HashMap::new(),
            diff_review_previous_mode: None,
            diff_review_comparison_mode: crate::git::diff_review::DiffComparisonMode::default(),
            line_selection: None,
            current_file_path: None,
            buffer_versions: self.buffer_versions.clone(),
            lsp_manager: self.lsp_manager.clone(),
            worktree: self.worktree.clone(),
            parent_stoat: Some(parent_weak),
            display_map: self.display_map.clone(),
            select_next_state: None,
            select_prev_state: None,
            compiled_keymap: self.compiled_keymap.clone(),
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
