use crate::{
    command::{overlay::CommandOverlay, palette::CommandPalette},
    editor::view::EditorView,
    file_finder::Finder,
    git::status::GitStatus,
    modal::{about::AboutModal, help::HelpModal},
    pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection},
    pane_group::element::pane_axis,
    render_stats::{FrameTimer, RenderStatsOverlayElement},
    status_bar::StatusBar,
    stoat::{KeyContext, Stoat, StoatEvent},
};
use gpui::{
    div, prelude::FluentBuilder, AnyElement, App, AppContext, Context, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, Styled,
    Subscription, Window,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// Pixel offset to adjust the minimap thumb's Y position.
///
/// This constant compensates for padding alignment in the editor coordinate system.
/// The viewport calculation assumes lines start at Y=0, but the first visible line
/// actually starts at Y=4px (after padding). This causes the thumb to be positioned
/// slightly higher than the actual visible content.
///
/// Without this offset, the minimap thumb appeared ~2 lines off from the actual
/// visible content in the editor. Users reported that the thumb didn't align with
/// what was actually visible on screen.
const THUMB_OFFSET_PX: f64 = 2.0;

/// Pixel offset to adjust the minimap thumb's height.
///
/// This constant compensates for the discrepancy between calculated visible lines
/// and actually rendered lines. The viewport calculation includes fractional lines
/// (e.g., 45.2 lines fit in the viewport), but the renderer only shows complete
/// lines (45 lines). This causes the thumb to be sized for slightly more content
/// than is actually visible.
///
/// Without this offset, the thumb was slightly shorter than the actual visible
/// region, making it appear misaligned with the editor viewport. Combined with
/// [`THUMB_OFFSET_PX`], this solved the ~2 line positioning error.
const THUMB_HEIGHT_OFFSET_PX: f64 = 1.0;

/// Default threshold in lines for scroll hint mode.
///
/// Scroll changes smaller than this threshold won't trigger the minimap hint.
/// Set to 5 lines to prevent small movements (like single jk presses) from
/// causing the minimap to blink in and out.
pub(crate) const SCROLL_HINT_DEFAULT_THRESHOLD: f32 = 5.0;

/// Duration the minimap hint stays visible after a large scroll.
///
/// Allows users to orient themselves without being distracting.
const SCROLL_HINT_DURATION: Duration = Duration::from_millis(1000);

/// Duration for fade-in animation when minimap appears.
const FADE_IN_DURATION: Duration = Duration::from_millis(100);

/// Duration for fade-out animation when minimap disappears.
const FADE_OUT_DURATION: Duration = Duration::from_millis(300);

/// Minimap fade animation state.
///
/// Tracks the current fade animation state of the minimap in ScrollHint mode.
/// Transitions: Hidden -> FadingIn -> Visible -> FadingOut -> Hidden
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MinimapFadeState {
    /// Minimap is not rendered
    Hidden,
    /// Minimap is fading in (opacity 0.0 to 1.0)
    FadingIn { started_at: Instant },
    /// Minimap is fully visible (opacity 1.0)
    Visible { expires_at: Instant },
    /// Minimap is fading out (opacity 1.0 to 0.0)
    FadingOut { started_at: Instant },
}

impl Default for MinimapFadeState {
    fn default() -> Self {
        Self::Hidden
    }
}

/// Minimap visibility mode.
///
/// Controls when and how the minimap is displayed to the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinimapVisibility {
    /// Minimap is always visible
    AlwaysVisible,
    /// Minimap is always hidden
    AlwaysHidden,
    /// Minimap appears temporarily on large scrolls
    ///
    /// The minimap stays hidden until the viewport scrolls by more than
    /// the threshold (in lines). When triggered, it appears for
    /// [`SCROLL_HINT_DURATION`] then automatically hides.
    ScrollHint {
        /// Scroll threshold in lines
        threshold_lines: f32,
    },
}

impl Default for MinimapVisibility {
    fn default() -> Self {
        Self::AlwaysVisible
    }
}

/// Main view that manages multiple editor panes in a tree layout.
///
/// PaneGroupView wraps a [`PaneGroup`] (from stoat core) and maintains
/// the mapping from [`PaneId`] to [`PaneContent`] (which can hold different
/// view types). It handles split operations, pane focus, and recursive
/// rendering of the pane tree.
///
/// The minimap is owned at this level (window-level) rather than per-pane,
/// ensuring only one minimap appears regardless of split configuration.
pub struct PaneGroupView {
    /// Workspace-level state shared across all panes
    pub app_state: crate::app_state::AppState,
    pub(crate) pane_group: PaneGroup,
    pub(crate) pane_contents: HashMap<PaneId, crate::content_view::PaneContent>,
    pub(crate) active_pane: PaneId,
    focus_handle: FocusHandle,
    pub(crate) compiled_keymap: Arc<crate::keymap::compiled::CompiledKeymap>,
    file_finder_scroll: ScrollHandle,
    command_palette_scroll: ScrollHandle,
    buffer_finder_scroll: ScrollHandle,
    git_status_scroll: ScrollHandle,
    render_stats_tracker: Rc<RefCell<FrameTimer>>,
    /// Single minimap view for the entire window (updates to show active pane's content)
    minimap_view: Entity<EditorView>,
    /// Minimap visibility mode
    pub(crate) minimap_visibility: MinimapVisibility,
    /// Last editor scroll position (for detecting scroll changes in ScrollHint mode)
    pub(crate) last_editor_scroll_y: Option<f32>,
    /// Minimap fade animation state (for ScrollHint mode)
    pub(crate) minimap_fade_state: MinimapFadeState,
    /// Help overlay visibility (non-modal overlay showing hint to press ? again)
    pub(crate) help_overlay_visible: bool,
    /// Subscriptions to StoatEvent::Action on each pane's Stoat entity
    stoat_subscriptions: Vec<Subscription>,
    /// Pending actions queued from StoatEvent::Action, processed in render() (which has window
    /// access)
    pub(crate) pending_actions: Vec<(String, Vec<String>)>,
    activation_observer_set: bool,
}

impl PaneGroupView {
    /// Create a new pane group view with config and optional initial files.
    ///
    /// Creates workspace state first, then uses it to create the initial editor.
    /// This ensures all editors share the workspace's worktree and buffer_store.
    ///
    /// # Arguments
    ///
    /// * `config` - Global configuration
    /// * `initial_paths` - Optional files to load on startup
    /// * `keymap` - Keymap for keybinding resolution
    /// * `cx` - GPUI context
    pub fn new(
        config: crate::Config,
        initial_paths: Vec<std::path::PathBuf>,
        compiled_keymap: Arc<crate::keymap::compiled::CompiledKeymap>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        // Create workspace state first (this creates worktree and buffer_store)
        let app_state = crate::app_state::AppState::new(cx);

        // Create initial Stoat using workspace's shared resources
        let initial_stoat = cx.new(|cx| {
            let mut stoat = Stoat::new(
                config.clone(),
                app_state.worktree.clone(),
                app_state.buffer_store.clone(),
                Some(app_state.lsp_manager.clone()),
                compiled_keymap.clone(),
                cx,
            );

            // Load first file if provided
            if !initial_paths.is_empty() {
                if let Err(e) = stoat.load_file(&initial_paths[0], cx) {
                    tracing::error!("Failed to load file: {}", e);
                }
            }

            stoat
        });

        // Create initial EditorView
        let initial_editor = cx.new(|cx| EditorView::new(initial_stoat, cx));

        // Set entity reference so EditorView can pass it to EditorElement
        initial_editor.update(cx, |view, _| {
            view.set_entity(initial_editor.clone());
        });

        let pane_group = PaneGroup::new();
        let initial_pane_id = pane_group.panes()[0];

        let mut pane_contents = HashMap::new();
        pane_contents.insert(
            initial_pane_id,
            crate::content_view::PaneContent::Editor(initial_editor.clone()),
        );

        // Create single minimap for the entire window
        // The minimap shares the initial editor's Stoat and will be updated when active pane
        // changes
        let minimap_view = {
            let initial_stoat = initial_editor.read(cx).stoat.clone();
            let minimap_stoat = initial_stoat.update(cx, |stoat, cx| stoat.create_minimap(cx));

            // Create minimap-specific style with tiny font (following Zed's architecture)
            let minimap_font = gpui::Font {
                family: gpui::SharedString::from("Menlo"),
                features: Default::default(),
                weight: gpui::FontWeight(crate::minimap::MINIMAP_FONT_WEIGHT), // BLACK (900)
                style: gpui::FontStyle::Normal,
                fallbacks: None,
            };

            // Get config from the stoat to create base style, then override minimap-specific
            // settings
            let config = initial_stoat.read(cx).config().clone();
            let mut minimap_style = crate::editor::style::EditorStyle::new(&config);
            minimap_style.font_size = gpui::px(crate::minimap::MINIMAP_FONT_SIZE); // 2.0px
            minimap_style.line_height = gpui::px(crate::minimap::MINIMAP_LINE_HEIGHT); // 2.5px
            minimap_style.font = minimap_font;
            minimap_style.show_line_numbers = false;
            minimap_style.show_diff_indicators = false;
            minimap_style.show_minimap = false; // Minimap doesn't render its own minimap
            let minimap_style = std::sync::Arc::new(minimap_style);

            // Create minimap EditorView with custom style
            let minimap_view = cx.new(|cx| {
                let mut editor = EditorView::new(minimap_stoat, cx);
                // Override the editor style with minimap-specific settings
                editor.editor_style = minimap_style;
                editor
            });

            // Set entity reference so EditorView can pass it to EditorElement
            minimap_view.update(cx, |view, _| {
                view.set_entity(minimap_view.clone());
            });

            minimap_view
        };

        let initial_stoat_ref = initial_editor.read(cx).stoat.clone();
        let initial_sub =
            cx.subscribe(
                &initial_stoat_ref,
                |this: &mut Self, _stoat, event, cx| match event {
                    StoatEvent::Action { name, args } => {
                        this.pending_actions.push((name.clone(), args.clone()));
                        cx.notify();
                    },
                    StoatEvent::FileOpened { language } => {
                        this.app_state
                            .ensure_lsp_for_language(*language, cx.weak_entity(), cx);
                    },
                    _ => {},
                },
            );

        Self {
            app_state,
            pane_group,
            pane_contents,
            active_pane: initial_pane_id,
            focus_handle: cx.focus_handle(),
            compiled_keymap,
            file_finder_scroll: ScrollHandle::new(),
            command_palette_scroll: ScrollHandle::new(),
            buffer_finder_scroll: ScrollHandle::new(),
            git_status_scroll: ScrollHandle::new(),
            render_stats_tracker: Rc::new(RefCell::new(FrameTimer::new())),
            minimap_view,
            minimap_visibility: MinimapVisibility::AlwaysVisible,
            last_editor_scroll_y: None,
            minimap_fade_state: MinimapFadeState::Hidden,
            help_overlay_visible: false,
            stoat_subscriptions: vec![initial_sub],
            pending_actions: Vec::new(),
            activation_observer_set: false,
        }
    }

    /// Get the active editor view
    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.pane_contents
            .get(&self.active_pane)
            .and_then(|content| content.as_editor())
    }

    /// Focus the currently active editor.
    ///
    /// This should be called after creating the [`PaneGroupView`] to establish the initial
    /// focus, ensuring keyboard input is routed to the active editor. Used by [`run_with_paths`]
    /// during app initialization.
    pub fn focus_active_editor(&self, window: &mut Window, cx: &App) {
        if let Some(editor) = self.active_editor() {
            window.focus(&editor.read(cx).focus_handle(cx));
        }
    }

    /// Exit Pane mode if currently in it, returning to Normal mode.
    ///
    /// This is called after pane commands execute to make Pane mode a one-shot mode.
    pub(crate) fn exit_pane_mode(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(editor) = self
            .pane_contents
            .get_mut(&self.active_pane)
            .and_then(|content| content.as_editor())
        {
            editor.update(cx, |editor, cx| {
                let mode = editor.stoat.read(cx).mode().to_string();
                if mode == "pane" {
                    editor.stoat.update(cx, |stoat, _| {
                        stoat.set_mode("normal");
                    });
                    cx.notify();
                }
            });
        }
    }

    /// Update the minimap to show the active pane's content.
    ///
    /// This should be called whenever the active pane changes (focus, split, close).
    /// The minimap's Stoat will be updated to point to the same buffer as the active editor.
    pub(crate) fn update_minimap_to_active_pane(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(active_editor) = self
            .pane_contents
            .get(&self.active_pane)
            .and_then(|content| content.as_editor())
        {
            let active_stoat = active_editor.read(cx).stoat.clone();

            // Update minimap's Stoat to point to the active editor's buffer
            self.minimap_view.update(cx, |minimap_view, cx| {
                // Create a new minimap Stoat from the active editor's Stoat
                let new_minimap_stoat =
                    active_stoat.update(cx, |stoat, cx| stoat.create_minimap(cx));
                minimap_view.stoat = new_minimap_stoat;
                cx.notify();
            });
        }
    }

    pub(crate) fn subscribe_to_stoat(&mut self, stoat: &Entity<Stoat>, cx: &mut Context<'_, Self>) {
        let sub = cx.subscribe(stoat, |this: &mut Self, _stoat, event, cx| match event {
            StoatEvent::Action { name, args } => {
                this.pending_actions.push((name.clone(), args.clone()));
                cx.notify();
            },
            StoatEvent::FileOpened { language } => {
                this.app_state
                    .ensure_lsp_for_language(*language, cx.weak_entity(), cx);
            },
            _ => {},
        });
        self.stoat_subscriptions.push(sub);
    }

    pub(crate) fn dispatch_action_by_name(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use crate::keymap::dispatch::dispatch_editor_action;
        use stoat_config::{Action, ActionExpr};

        let action = ActionExpr::Single(Action {
            name: name.to_string(),
            args: vec![],
        });

        // Try editor-level dispatch first
        if let Some(editor) = self.active_editor().cloned() {
            let stoat = editor.read(cx).stoat.clone();
            if dispatch_editor_action(&stoat, &action, cx) {
                cx.notify();
                return;
            }
        }

        // Fall through to pane-level dispatch
        self.pending_actions.push((name.to_string(), vec![]));
        self.process_pending_actions(window, cx);
    }

    fn process_pending_actions(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let actions = std::mem::take(&mut self.pending_actions);
        for (name, _args) in actions {
            match name.as_str() {
                "SplitUp" => self.handle_split_up(window, cx),
                "SplitDown" => self.handle_split_down(window, cx),
                "SplitLeft" => self.handle_split_left(window, cx),
                "SplitRight" => self.handle_split_right(window, cx),
                "Quit" | "ClosePane" => self.handle_quit(window, cx),
                "FocusPaneUp" => self.handle_focus_pane_up(window, cx),
                "FocusPaneDown" => self.handle_focus_pane_down(window, cx),
                "FocusPaneLeft" => self.handle_focus_pane_left(window, cx),
                "FocusPaneRight" => self.handle_focus_pane_right(window, cx),
                "OpenFileFinder" => self.handle_open_file_finder(window, cx),
                "FileFinderNext" => self.handle_file_finder_next(window, cx),
                "FileFinderPrev" => self.handle_file_finder_prev(window, cx),
                "FileFinderSelect" => self.handle_file_finder_select(window, cx),
                "FileFinderDismiss" => self.handle_file_finder_dismiss(window, cx),
                "OpenCommandPalette" => self.handle_open_command_palette(window, cx),
                "CommandPaletteNext" => self.handle_command_palette_next(window, cx),
                "CommandPalettePrev" => self.handle_command_palette_prev(window, cx),
                "CommandPaletteExecute" => self.handle_command_palette_execute(window, cx),
                "CommandPaletteDismiss" => self.handle_command_palette_dismiss(window, cx),
                "ToggleCommandPaletteHidden" => {
                    self.handle_command_palette_toggle_hidden(window, cx);
                },
                "OpenCommandPaletteV2" => self.handle_open_command_palette_v2(window, cx),
                "DismissCommandPaletteV2" => {
                    self.handle_dismiss_command_palette_v2(window, cx);
                },
                "AcceptCommandPaletteV2" => {
                    self.handle_accept_command_palette_v2(window, cx);
                },
                "SelectNextCommandV2" => self.handle_select_next_command_v2(window, cx),
                "SelectPrevCommandV2" => self.handle_select_prev_command_v2(window, cx),
                "OpenBufferFinder" => self.handle_open_buffer_finder(window, cx),
                "BufferFinderNext" => self.handle_buffer_finder_next(window, cx),
                "BufferFinderPrev" => self.handle_buffer_finder_prev(window, cx),
                "BufferFinderSelect" => self.handle_buffer_finder_select(window, cx),
                "BufferFinderDismiss" => self.handle_buffer_finder_dismiss(window, cx),
                "OpenGitStatus" => self.handle_open_git_status(window, cx),
                "GitStatusNext" => self.handle_git_status_next(window, cx),
                "GitStatusPrev" => self.handle_git_status_prev(window, cx),
                "GitStatusSelect" => self.handle_git_status_select(window, cx),
                "GitStatusDismiss" => self.handle_git_status_dismiss(window, cx),
                "GitStatusCycleFilter" => self.handle_git_status_cycle_filter(window, cx),
                "GitStatusSetFilterAll" => self.handle_git_status_set_filter_all(window, cx),
                "GitStatusSetFilterStaged" => {
                    self.handle_git_status_set_filter_staged(window, cx);
                },
                "GitStatusSetFilterUnstaged" => {
                    self.handle_git_status_set_filter_unstaged(window, cx);
                },
                "GitStatusSetFilterUnstagedWithUntracked" => {
                    self.handle_git_status_set_filter_unstaged_with_untracked(window, cx);
                },
                "GitStatusSetFilterUntracked" => {
                    self.handle_git_status_set_filter_untracked(window, cx);
                },
                "OpenDiffReview" => self.handle_open_diff_review(window, cx),
                "OpenHelpOverlay" => self.handle_open_help_overlay(window, cx),
                "OpenHelpModal" => self.handle_open_help_modal(window, cx),
                "HelpModalDismiss" => self.handle_help_modal_dismiss(window, cx),
                "OpenAboutModal" => self.handle_open_about_modal(window, cx),
                "AboutModalDismiss" => self.handle_about_modal_dismiss(window, cx),
                "ToggleMinimap" => self.handle_toggle_minimap(window, cx),
                "ShowMinimapOnScroll" => self.handle_show_minimap_on_scroll(window, cx),
                "ShowCommandLine" => self.handle_show_command_line(window, cx),
                "CommandLineDismiss" => self.handle_command_line_dismiss(window, cx),
                "ChangeDirectory" => {
                    if let Some(path) = _args.first() {
                        self.handle_change_directory(&std::path::PathBuf::from(path), window, cx);
                    }
                },
                "PrintWorkingDirectory" => {
                    self.handle_print_working_directory(window, cx);
                },
                "QuitAll" => cx.quit(),
                _ => {
                    tracing::warn!("Unknown pane action: {}", name);
                },
            }
        }
    }

    /// Load preview for the currently selected file in file finder.
    ///
    /// Spawns an async task to load file preview. Updates app state.file_finder.preview
    /// when complete. This method follows the same pattern as Stoat's load_preview_for_selected
    /// but operates on workspace state instead.
    pub(crate) fn load_file_finder_preview(&mut self, cx: &mut Context<'_, Self>) {
        // Cancel existing preview task
        self.app_state.file_finder.preview_task = None;

        // Get selected file path from workspace
        let relative_path = match self
            .app_state
            .file_finder
            .filtered
            .get(self.app_state.file_finder.selected)
        {
            Some(path) => path.clone(),
            None => {
                self.app_state.file_finder.preview = None;
                return;
            },
        };

        // Build absolute path
        let root = self
            .app_state
            .worktree
            .lock()
            .snapshot()
            .root()
            .to_path_buf();
        let abs_path = root.join(&relative_path);
        let abs_path_for_highlight = abs_path.clone();

        // Spawn async task to load preview
        self.app_state.file_finder.preview_task = Some(cx.spawn(async move |this, cx| {
            // Phase 1: Load plain text immediately
            if let Some(text) = crate::file_finder::load_text_only(&abs_path).await {
                let _ = this.update(cx, |pane_group, cx| {
                    pane_group.app_state.file_finder.preview =
                        Some(crate::file_finder::PreviewData::Plain(text));
                    cx.notify();
                });
            }

            // Phase 2: Load syntax-highlighted version
            if let Some(highlighted) =
                crate::file_finder::load_file_preview(&abs_path_for_highlight).await
            {
                let _ = this.update(cx, |pane_group, cx| {
                    pane_group.app_state.file_finder.preview = Some(highlighted);
                    cx.notify();
                });
            }
        }));
    }

    /// Filter command palette commands based on query and show_hidden flag.
    ///
    /// Updates [`AppState::command_palette::filtered`] with commands that match
    /// the fuzzy search query and respect the show_hidden setting. Uses nucleo_matcher
    /// for fuzzy matching.
    pub(crate) fn filter_command_palette_commands(&mut self, query: &str) {
        use nucleo_matcher::{
            pattern::{CaseMatching, Normalization, Pattern},
            Matcher,
        };

        let show_hidden = self.app_state.command_palette.show_hidden;
        let all_commands = &self.app_state.command_palette.commands;

        if query.is_empty() {
            // No query - show all commands (filtered by hidden state)
            self.app_state.command_palette.filtered = all_commands
                .iter()
                .filter(|cmd| show_hidden || !cmd.hidden)
                .cloned()
                .collect();
        } else {
            // Fuzzy match query against command name/description using nucleo_matcher
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);

            // Build haystack strings and keep parallel vector of commands for lookup
            let visible_commands: Vec<crate::CommandInfo> = all_commands
                .iter()
                .filter(|cmd| show_hidden || !cmd.hidden)
                .cloned()
                .collect();

            let candidates: Vec<String> = visible_commands
                .iter()
                .map(|cmd| format!("{} {} {}", cmd.name, cmd.description, cmd.aliases.join(" ")))
                .collect();

            // Match and collect results
            let candidate_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
            let mut matches = pattern.match_list(candidate_refs, &mut matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by score descending

            // Map matched strings back to commands using the parallel vector
            self.app_state.command_palette.filtered = matches
                .into_iter()
                .filter_map(|(matched_str, _score)| {
                    // Find the command that produced this matched string
                    candidates
                        .iter()
                        .position(|s| s.as_str() == matched_str)
                        .and_then(|idx| visible_commands.get(idx).cloned())
                })
                .collect();
        }
    }

    fn filter_file_finder_files(&mut self, query: &str) {
        use nucleo_matcher::{
            pattern::{CaseMatching, Normalization, Pattern},
            Matcher,
        };

        let all_files = &self.app_state.file_finder.files;

        if query.is_empty() {
            self.app_state.file_finder.filtered = all_files
                .iter()
                .map(|e| PathBuf::from(e.path.as_unix_str()))
                .collect();
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);

            let candidates: Vec<String> = all_files
                .iter()
                .map(|e| e.path.as_unix_str().to_string())
                .collect();

            let candidate_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
            let mut matches = pattern.match_list(candidate_refs, &mut matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.app_state.file_finder.filtered = matches
                .into_iter()
                .map(|(matched_str, _score)| PathBuf::from(matched_str))
                .collect();
        }
    }

    /// Load preview for the currently selected file in git status.
    ///
    /// Spawns an async task to load git diff preview. Updates app state.git_status.preview
    /// when complete.
    pub(crate) fn load_git_status_preview(&mut self, cx: &mut Context<'_, Self>) {
        // Cancel existing preview task
        self.app_state.git_status.preview_task = None;

        // Get selected file entry from filtered list
        let entry = match self
            .app_state
            .git_status
            .filtered
            .get(self.app_state.git_status.selected)
        {
            Some(entry) => entry.clone(),
            None => {
                self.app_state.git_status.preview = None;
                return;
            },
        };

        // Get repository root path
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let file_path = entry.path.clone();

        // Spawn async task to load diff
        self.app_state.git_status.preview_task = Some(cx.spawn(async move |this, cx| {
            // Load git diff
            if let Some(diff) = crate::git::status::load_git_diff(&root_path, &file_path).await {
                // Update workspace through entity handle
                let _ = this.update(cx, |pane_group, cx| {
                    pane_group.app_state.git_status.preview = Some(diff);
                    cx.notify();
                });
            }
        }));
    }

    /// Split the active pane in the given direction.
    ///
    /// This is public so it can be called from actions with access to Window context.
    pub fn split(
        &mut self,
        direction: SplitDirection,
        new_editor: Entity<EditorView>,
        _cx: &mut Context<'_, Self>,
    ) {
        let new_pane_id = self.pane_group.split(self.active_pane, direction);
        self.pane_contents.insert(
            new_pane_id,
            crate::content_view::PaneContent::Editor(new_editor),
        );
        self.active_pane = new_pane_id;
    }

    /// Get the pane in the given direction (simplified tree-order navigation)
    pub(crate) fn get_pane_in_direction(&self, direction: SplitDirection) -> Option<PaneId> {
        let all_panes = self.pane_group.panes();
        if all_panes.len() <= 1 {
            return None;
        }

        let current_idx = all_panes.iter().position(|&p| p == self.active_pane)?;

        match direction {
            SplitDirection::Left | SplitDirection::Up => {
                // Previous pane (wrap around)
                if current_idx > 0 {
                    Some(all_panes[current_idx - 1])
                } else {
                    Some(all_panes[all_panes.len() - 1])
                }
            },
            SplitDirection::Right | SplitDirection::Down => {
                // Next pane (wrap around)
                if current_idx < all_panes.len() - 1 {
                    Some(all_panes[current_idx + 1])
                } else {
                    Some(all_panes[0])
                }
            },
        }
    }

    /// Compute minimap opacity based on current fade state.
    ///
    /// Returns opacity value (0.0 to 1.0) and whether animation is active.
    /// Animation is active during FadingIn and FadingOut states.
    fn calculate_minimap_opacity(&mut self) -> (f32, bool) {
        match self.minimap_visibility {
            MinimapVisibility::AlwaysVisible => (1.0, false),
            MinimapVisibility::AlwaysHidden => (0.0, false),
            MinimapVisibility::ScrollHint { .. } => match self.minimap_fade_state {
                MinimapFadeState::Hidden => (0.0, false),
                MinimapFadeState::FadingIn { started_at } => {
                    let elapsed = started_at.elapsed();
                    let progress =
                        (elapsed.as_secs_f32() / FADE_IN_DURATION.as_secs_f32()).min(1.0);

                    if progress >= 1.0 {
                        // Fade-in complete, transition to Visible
                        // (This shouldn't happen often as timer should handle it, but just in case)
                        let expires_at = Instant::now() + SCROLL_HINT_DURATION;
                        self.minimap_fade_state = MinimapFadeState::Visible { expires_at };
                        (1.0, false)
                    } else {
                        (progress, true)
                    }
                },
                MinimapFadeState::Visible { .. } => (1.0, false),
                MinimapFadeState::FadingOut { started_at } => {
                    let elapsed = started_at.elapsed();
                    let progress =
                        (elapsed.as_secs_f32() / FADE_OUT_DURATION.as_secs_f32()).min(1.0);

                    if progress >= 1.0 {
                        // Fade-out complete, transition to Hidden
                        self.minimap_fade_state = MinimapFadeState::Hidden;
                        (0.0, false)
                    } else {
                        (1.0 - progress, true)
                    }
                },
            },
        }
    }

    /// Recursively render a member of the pane tree.
    fn render_member(&self, member: &Member, basis: usize) -> AnyElement {
        match member {
            Member::Pane(pane_id) => {
                if let Some(content) = self.pane_contents.get(pane_id) {
                    match content {
                        crate::content_view::PaneContent::Editor(editor) => div()
                            .flex_1()
                            .size_full()
                            .child(editor.clone())
                            .into_any_element(),
                        crate::content_view::PaneContent::Static(static_view) => div()
                            .flex_1()
                            .size_full()
                            .child(static_view.clone())
                            .into_any_element(),
                    }
                } else {
                    div()
                        .flex_1()
                        .size_full()
                        .child("Missing pane")
                        .into_any_element()
                }
            },
            Member::Axis(axis) => self.render_axis(axis, basis),
        }
    }

    /// Render an axis with its children using PaneAxisElement for interactive resize.
    fn render_axis(&self, axis: &PaneAxis, basis: usize) -> AnyElement {
        let mut element = pane_axis(
            axis.axis,
            basis,
            axis.flexes.clone(),
            axis.bounding_boxes.clone(),
        );

        for member in &axis.members {
            element = element.child(self.render_member(member, basis + 1));
        }

        element.into_any_element()
    }
}

impl Focusable for PaneGroupView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaneGroupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if !self.activation_observer_set {
            self.activation_observer_set = true;
            let sub = cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    return;
                }
                if let Some(editor) = this.active_editor().cloned() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, cx| {
                            stoat.refresh_git_diff(cx);
                        });
                    });
                }
            });
            self.stoat_subscriptions.push(sub);
        }

        // Process any pending actions from StoatEvent::Action subscriptions
        if !self.pending_actions.is_empty() {
            self.process_pending_actions(window, cx);
        }

        // Track scroll position for ScrollHint mode
        // Extract early to avoid borrow conflicts with later code
        let current_scroll_y = self
            .pane_contents
            .get(&self.active_pane)
            .and_then(|content| content.as_editor())
            .map(|editor| {
                let stoat = editor.read(cx).stoat.read(cx);
                stoat.scroll_position().y
            });

        // Update scroll hint state if in ScrollHint mode
        if let MinimapVisibility::ScrollHint { threshold_lines } = self.minimap_visibility {
            if let Some(current_y) = current_scroll_y {
                // Check if scroll exceeds threshold
                let scroll_delta = self
                    .last_editor_scroll_y
                    .map(|last_y| (current_y - last_y).abs())
                    .unwrap_or(0.0);

                if scroll_delta >= threshold_lines {
                    // Large scroll detected - show minimap or extend visibility
                    match self.minimap_fade_state {
                        MinimapFadeState::Hidden => {
                            // Start fade-in animation from hidden state
                            let now = Instant::now();
                            self.minimap_fade_state =
                                MinimapFadeState::FadingIn { started_at: now };

                            // Spawn timer to transition through visible state to fade-out
                            cx.spawn(async move |this, cx| {
                                // Wait for fade-in to complete
                                cx.background_executor().timer(FADE_IN_DURATION).await;

                                // Transition to Visible state
                                let fade_in_completed = this.update(cx, |this, _cx| {
                                    if matches!(
                                        this.minimap_fade_state,
                                        MinimapFadeState::FadingIn { .. }
                                    ) {
                                        let expires_at = Instant::now() + SCROLL_HINT_DURATION;
                                        this.minimap_fade_state =
                                            MinimapFadeState::Visible { expires_at };
                                        true
                                    } else {
                                        // State changed (new scroll), abort this transition
                                        false
                                    }
                                });

                                if fade_in_completed.unwrap_or(false) {
                                    // Wait for visible duration
                                    cx.background_executor().timer(SCROLL_HINT_DURATION).await;

                                    // Transition to FadingOut state
                                    this.update(cx, |this, cx| {
                                        if matches!(
                                            this.minimap_fade_state,
                                            MinimapFadeState::Visible { .. }
                                        ) {
                                            this.minimap_fade_state = MinimapFadeState::FadingOut {
                                                started_at: Instant::now(),
                                            };
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                                }
                            })
                            .detach();
                        },
                        MinimapFadeState::FadingIn { .. }
                        | MinimapFadeState::Visible { .. }
                        | MinimapFadeState::FadingOut { .. } => {
                            // Already visible or animating - keep it visible and restart timer
                            let expires_at = Instant::now() + SCROLL_HINT_DURATION;
                            self.minimap_fade_state = MinimapFadeState::Visible { expires_at };

                            // Spawn timer for new fade-out
                            cx.spawn(async move |this, cx| {
                                // Wait for visible duration
                                cx.background_executor().timer(SCROLL_HINT_DURATION).await;

                                // Transition to FadingOut state
                                this.update(cx, |this, cx| {
                                    // Only fade out if still in the expected Visible state
                                    if matches!(
                                        this.minimap_fade_state,
                                        MinimapFadeState::Visible { expires_at }
                                        if expires_at <= Instant::now()
                                    ) {
                                        this.minimap_fade_state = MinimapFadeState::FadingOut {
                                            started_at: Instant::now(),
                                        };
                                        cx.notify();
                                    }
                                })
                                .ok();
                            })
                            .detach();
                        },
                    }

                    cx.notify();
                }

                // Update tracked position
                self.last_editor_scroll_y = Some(current_y);
            }
        }

        // Compute minimap opacity and check if animating
        let (minimap_opacity, is_animating) = self.calculate_minimap_opacity();
        let minimap_visible = minimap_opacity > 0.0;

        // Request animation frame if currently animating
        if is_animating {
            window.request_animation_frame();
        }

        // Extract minimap viewport lines before main data extraction to avoid borrow conflicts
        // Only compute if minimap is visible to avoid performance impact
        let minimap_viewport_lines = if minimap_visible {
            self.minimap_view.read(cx).stoat.read(cx).viewport_lines()
        } else {
            None
        };

        // Request another frame if minimap is visible but hasn't rendered yet
        // This ensures thumb appears on the next frame after pane changes
        if minimap_visible && minimap_viewport_lines.is_none() {
            window.request_animation_frame();
        }

        // Get the key context, mode, file finder data, command palette data, buffer finder data,
        // git status data, status bar data, minimap scroll, and thumb data from the active editor
        let (
            key_context,
            active_mode,
            mode_display,
            _file_finder_data,
            _command_palette_data,
            buffer_finder_data,
            _git_status_data,
            status_bar_data,
            minimap_scroll_to_set,
            thumb_calculation_data,
        ) = self
            .pane_contents
            .get(&self.active_pane)
            .and_then(|content| content.as_editor())
            .map(|editor| {
                let stoat_entity = editor.read(cx).stoat.clone();
                let stoat = stoat_entity.read(cx);
                let key_context = stoat.key_context();
                let mode_name = stoat.mode();
                let mut display = stoat
                    .get_mode(mode_name)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_else(|| mode_name.to_uppercase());
                if let Some(sel) = &stoat.line_selection {
                    display = format!(
                        "{display} ({}/{})",
                        sel.selected_count(),
                        sel.total_changeable_count()
                    );
                }

                // File finder data is now extracted from workspace state below
                let ff_data = None::<(
                    String,
                    Vec<PathBuf>,
                    usize,
                    Option<crate::file_finder::PreviewData>,
                )>;

                // Command palette data is now extracted from workspace state below
                let cp_data = None::<(String, Vec<crate::CommandInfo>, usize)>;

                // Buffer finder data is now extracted from workspace state below
                let bf_data = None::<(String, Vec<crate::buffer::store::BufferListEntry>, usize)>;

                // Extract git status data from workspace (not from Stoat)
                let gs_data = None::<(
                    Vec<crate::git::status::GitStatusEntry>,
                    Vec<crate::git::status::GitStatusEntry>,
                    crate::git::status::GitStatusFilter,
                    usize,
                    usize,
                    Option<crate::git::status::DiffPreviewData>,
                    Option<crate::git::status::GitBranchInfo>,
                )>;

                // Extract status bar data
                let sb_data = (
                    display.clone(),
                    None::<crate::git::status::GitBranchInfo>, // Will be set from workspace below
                    Vec::<crate::git::status::GitStatusEntry>::new(), /* Will be set from
                                                                * workspace below */
                    stoat.current_file_path().map(|p| p.display().to_string()),
                    stoat.diff_review_progress(),
                    stoat.diff_review_file_progress(cx),
                    stoat.diff_review_hunk_position(cx),
                    if mode_name == "diff_review" || mode_name == "line_select" {
                        Some((stoat.review_scope, stoat.review_state.filter))
                    } else {
                        None
                    },
                    self.app_state.lsp_state.status.read().display_string(),
                );

                // Calculate minimap scroll position for later update
                // Only compute if minimap is visible to avoid performance impact
                let minimap_scroll = if minimap_visible {
                    let buffer_item = stoat.active_buffer(cx);
                    let buffer = buffer_item.read(cx).buffer().read(cx);
                    let buffer_snapshot = buffer.snapshot();
                    let total_lines = buffer_snapshot.max_point().row as f64 + 1.0;

                    stoat.viewport_lines().and_then(|visible_editor_lines| {
                        let editor_scroll_y = stoat.scroll_position().y as f64;

                        minimap_viewport_lines.map(|visible_minimap_lines| {
                            crate::minimap::MinimapLayout::calculate_minimap_scroll(
                                total_lines,
                                visible_editor_lines as f64,
                                visible_minimap_lines as f64,
                                editor_scroll_y,
                            )
                        })
                    })
                } else {
                    None
                };

                // Extract thumb calculation data (visible lines and editor scroll)
                // Only compute if minimap is visible to avoid performance impact
                let thumb_data = if minimap_visible {
                    stoat.viewport_lines().map(|visible_editor_lines| {
                        let editor_scroll_y = stoat.scroll_position().y;
                        (visible_editor_lines, editor_scroll_y)
                    })
                } else {
                    None
                };

                (
                    key_context,
                    mode_name.to_string(), // Convert to owned String to break borrow dependency
                    display,
                    ff_data,
                    cp_data,
                    bf_data,
                    gs_data,
                    Some(sb_data),
                    minimap_scroll,
                    thumb_data,
                )
            })
            .unwrap_or((
                KeyContext::TextEditor,
                "normal".to_string(),
                "NORMAL".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ));

        // Extract file finder data from workspace if in FileFinder context
        let file_finder_data = if key_context == KeyContext::FileFinder {
            let query = self
                .app_state
                .file_finder
                .input
                .as_ref()
                .map(|buffer| {
                    let buffer_snapshot = buffer.read(cx).snapshot();
                    buffer_snapshot.text()
                })
                .unwrap_or_default();

            self.filter_file_finder_files(&query);

            Some((
                query,
                self.app_state.file_finder.filtered.clone(),
                self.app_state.file_finder.selected,
                self.app_state.file_finder.preview.clone(),
            ))
        } else {
            None
        };

        // Extract command palette data from workspace if in CommandPalette context
        let command_palette_data = if key_context == KeyContext::CommandPalette {
            let query = self
                .app_state
                .command_palette
                .input
                .as_ref()
                .map(|buffer| {
                    let buffer_snapshot = buffer.read(cx).snapshot();
                    buffer_snapshot.text()
                })
                .unwrap_or_default();

            // Re-filter commands based on current query
            self.filter_command_palette_commands(&query);

            Some((
                query,
                self.app_state.command_palette.filtered.clone(),
                self.app_state.command_palette.selected,
            ))
        } else {
            None
        };

        // Extract git status data from workspace if in Git context
        let git_status_data = if key_context == KeyContext::Git {
            Some((
                self.app_state.git_status.files.clone(),
                self.app_state.git_status.filtered.clone(),
                self.app_state.git_status.filter,
                self.app_state.git_status.dirty_count,
                self.app_state.git_status.selected,
                self.app_state.git_status.preview.clone(),
                self.app_state.git_status.branch_info.clone(),
            ))
        } else {
            None
        };

        // Update status_bar_data with workspace git_status data
        let status_bar_data = status_bar_data.map(
            |(
                mode,
                _branch,
                _files,
                path,
                review_progress,
                review_file_progress,
                hunk_position,
                scope_filter,
                lsp_status,
            )| {
                (
                    mode,
                    self.app_state.git_status.branch_info.clone(),
                    self.app_state.git_status.files.clone(),
                    path,
                    review_progress,
                    review_file_progress,
                    hunk_position,
                    scope_filter,
                    lsp_status,
                )
            },
        );

        // Update minimap scroll position to match calculated value
        // This must happen after data extraction (to avoid borrow conflicts) but before thumb
        // calculation
        // Only update if minimap is visible to avoid performance impact
        if minimap_visible {
            if let Some(minimap_scroll_y) = minimap_scroll_to_set {
                self.minimap_view.update(cx, |minimap_view, cx| {
                    minimap_view.stoat.update(cx, |stoat, _cx| {
                        stoat.set_scroll_position(gpui::point(0.0, minimap_scroll_y));
                    });
                });
            }
        }

        // Query keymap for bindings in the current mode
        let bindings = crate::keymap::query::bindings_for_mode(&self.compiled_keymap, &active_mode);

        // Calculate minimap thumb bounds using pre-extracted data
        // Following Zed's architecture: thumb is sized and positioned using minimap line heights
        let minimap_thumb_bounds = minimap_scroll_to_set.and_then(|minimap_scroll_y| {
            thumb_calculation_data.map(|(visible_editor_lines, editor_scroll_y)| {
                // Calculate thumb using minimap line heights (following Zed's approach)
                let minimap_line_height = crate::minimap::MINIMAP_LINE_HEIGHT as f64;

                // Thumb height: visible_editor_lines × minimap_line_height
                // visible_editor_lines now reflects the actual rendered count from prepaint
                let thumb_height_px = visible_editor_lines as f64 * minimap_line_height;

                // Apply height offset (see THUMB_HEIGHT_OFFSET_PX module constant)
                let thumb_height_px_adjusted = thumb_height_px + THUMB_HEIGHT_OFFSET_PX;

                // Thumb Y position: (editor_scroll - minimap_scroll) × minimap_line_height
                let thumb_y_px =
                    (editor_scroll_y as f64 - minimap_scroll_y as f64) * minimap_line_height;

                // Apply position offset (see THUMB_OFFSET_PX module constant)
                let thumb_y_px_adjusted = thumb_y_px + THUMB_OFFSET_PX;

                gpui::Bounds {
                    origin: gpui::point(gpui::px(0.0), gpui::px(thumb_y_px_adjusted as f32)),
                    size: gpui::size(gpui::px(120.0), gpui::px(thumb_height_px_adjusted as f32)),
                }
            })
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .relative() // Enable absolute positioning for overlay
                    .track_focus(&self.focus_handle)
                    .child(self.render_member(self.pane_group.root(), 0))
                    .when(key_context == KeyContext::FileFinder, |div| {
                        // Render file finder overlay when in FileFinder context
                        if let Some((query, files, selected, preview)) = file_finder_data {
                            div.child(Finder::new_file_finder(
                                query,
                                files,
                                selected,
                                preview,
                                self.file_finder_scroll.clone(),
                            ))
                        } else {
                            div
                        }
                    })
                    .when(key_context == KeyContext::CommandPalette, |div| {
                        // Render command palette overlay when in CommandPalette context
                        if let Some((query, commands, selected)) = command_palette_data {
                            div.child(CommandPalette::new(
                                query,
                                commands,
                                selected,
                                self.command_palette_scroll.clone(),
                            ))
                        } else {
                            div
                        }
                    })
                    .when(key_context == KeyContext::CommandPaletteV2, |div| {
                        // Render command palette V2 overlay when in CommandPaletteV2 context
                        if let Some(palette) = &self.app_state.command_palette_v2 {
                            div.child(palette.clone())
                        } else {
                            div
                        }
                    })
                    .when(key_context == KeyContext::BufferFinder, |div| {
                        // Render buffer finder overlay when in BufferFinder context
                        if let Some((query, buffers, selected)) = buffer_finder_data {
                            div.child(Finder::new_buffer_finder(
                                query,
                                buffers,
                                selected,
                                self.buffer_finder_scroll.clone(),
                            ))
                        } else {
                            div
                        }
                    })
                    .when(key_context == KeyContext::Git, |div| {
                        // Render git status overlay when in Git context
                        if let Some((
                            files,
                            filtered,
                            filter,
                            total_count,
                            selected,
                            preview,
                            branch_info,
                        )) = git_status_data
                        {
                            div.child(GitStatus::new(
                                files,
                                filtered,
                                filter,
                                total_count,
                                selected,
                                preview,
                                branch_info,
                                self.git_status_scroll.clone(),
                            ))
                        } else {
                            div
                        }
                    })
                    .when(key_context == KeyContext::HelpModal, |div| {
                        // Render help modal when in HelpModal context
                        div.child(HelpModal::new())
                    })
                    .when(key_context == KeyContext::AboutModal, |div| {
                        // Render about modal when in AboutModal context
                        div.child(AboutModal::new())
                    })
                    // Render minimap as fixed overlay on the right side with opacity
                    // Only render if opacity > 0 (minimap_visible) to avoid performance impact
                    .when(minimap_visible, |parent_div| {
                        parent_div.child(
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .h_full()
                                .w(gpui::px(120.0)) // Fixed width in pixels
                                .opacity(minimap_opacity)
                                .child(self.minimap_view.clone()),
                        )
                    })
                    // Render minimap thumb (viewport indicator) if calculated, with same opacity
                    .when_some(minimap_thumb_bounds, |parent_div, thumb_bounds| {
                        parent_div.child(
                            div()
                                .absolute()
                                .occlude() // Allow pointer events to pass through
                                .right_0() // Aligned with minimap on right edge
                                .top(thumb_bounds.origin.y)
                                .w(gpui::px(120.0)) // Same width as minimap
                                .h(thumb_bounds.size.height)
                                .opacity(minimap_opacity)
                                .bg(gpui::rgba(0xFFFFFF22)) // Mostly clear white background
                                .border_1()
                                .border_color(gpui::rgba(0xFFFFFF55)), /* Semi-transparent white
                                                                        * border */
                        )
                    })
                    // Render help overlay (CommandOverlay) on top of minimap
                    .when(self.help_overlay_visible, |div| {
                        div.child(CommandOverlay::new(mode_display.clone(), bindings.clone()))
                    })
                    .child(RenderStatsOverlayElement::new(
                        self.render_stats_tracker.clone(),
                    )),
            )
            .when_some(
                status_bar_data,
                |div,
                 (
                    mode,
                    branch,
                    files,
                    path,
                    review_progress,
                    review_file_progress,
                    hunk_position,
                    scope_filter,
                    lsp_status,
                )| {
                    let flash_message = self.app_state.flash_message.clone();
                    div.child(StatusBar::new(
                        mode,
                        branch,
                        files,
                        path,
                        review_progress,
                        review_file_progress,
                        hunk_position,
                        scope_filter,
                        lsp_status,
                        flash_message,
                    ))
                },
            )
    }
}
