use crate::{
    about_modal::AboutModal,
    command_overlay::CommandOverlay,
    command_palette::CommandPalette,
    editor_view::EditorView,
    file_finder::Finder,
    git_status::GitStatus,
    help_modal::HelpModal,
    pane_group::element::pane_axis,
    render_stats::{FrameTimer, RenderStatsOverlayElement},
    status_bar::StatusBar,
};
use gpui::{
    div, prelude::FluentBuilder, AnyElement, App, AppContext, Context, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, Styled,
    Window,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};
use stoat::{
    actions::{
        AboutModalDismiss, ClosePane, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp,
        HelpModalDismiss, OpenAboutModal, OpenBufferFinder, OpenCommandPalette, OpenDiffReview,
        OpenFileFinder, OpenGitStatus, OpenHelpModal, OpenHelpOverlay, ShowMinimapOnScroll,
        SplitDown, SplitLeft, SplitRight, SplitUp, ToggleMinimap,
    },
    pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection},
    stoat::KeyContext,
    Stoat,
};
use tracing::debug;

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
const SCROLL_HINT_DEFAULT_THRESHOLD: f32 = 5.0;

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
enum MinimapFadeState {
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
/// the mapping from [`PaneId`] to [`EditorView`] entities. It handles
/// split operations, pane focus, and recursive rendering of the pane tree.
///
/// The minimap is owned at this level (window-level) rather than per-pane,
/// ensuring only one minimap appears regardless of split configuration.
pub struct PaneGroupView {
    pane_group: PaneGroup,
    pane_editors: HashMap<PaneId, Entity<EditorView>>,
    active_pane: PaneId,
    focus_handle: FocusHandle,
    keymap: Rc<gpui::Keymap>,
    file_finder_scroll: ScrollHandle,
    command_palette_scroll: ScrollHandle,
    buffer_finder_scroll: ScrollHandle,
    git_status_scroll: ScrollHandle,
    render_stats_tracker: Rc<RefCell<FrameTimer>>,
    /// Single minimap view for the entire window (updates to show active pane's content)
    minimap_view: Entity<EditorView>,
    /// Minimap visibility mode
    minimap_visibility: MinimapVisibility,
    /// Last editor scroll position (for detecting scroll changes in ScrollHint mode)
    last_editor_scroll_y: Option<f32>,
    /// Minimap fade animation state (for ScrollHint mode)
    minimap_fade_state: MinimapFadeState,
    /// Help overlay visibility (non-modal overlay showing hint to press ? again)
    help_overlay_visible: bool,
}

impl PaneGroupView {
    /// Create a new pane group view with an initial editor entity.
    ///
    /// The caller must create the initial EditorView entity using App context.
    pub fn new(
        initial_editor: Entity<EditorView>,
        keymap: Rc<gpui::Keymap>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let pane_group = PaneGroup::new();
        let initial_pane_id = pane_group.panes()[0];

        let mut pane_editors = HashMap::new();
        pane_editors.insert(initial_pane_id, initial_editor.clone());

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
            let mut minimap_style = crate::editor_style::EditorStyle::new(&config);
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

        Self {
            pane_group,
            pane_editors,
            active_pane: initial_pane_id,
            focus_handle: cx.focus_handle(),
            keymap,
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
        }
    }

    /// Get the active editor view
    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.pane_editors.get(&self.active_pane)
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
    fn exit_pane_mode(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(editor) = self.pane_editors.get_mut(&self.active_pane) {
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
    fn update_minimap_to_active_pane(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
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

    /// Handle opening the file finder
    fn handle_open_file_finder(
        &mut self,
        _: &OpenFileFinder,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Open file finder in the active editor's Stoat instance
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_file_finder(cx);
                });
            });
            cx.notify();
        }
    }

    /// Handle opening the command palette
    fn handle_open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Open command palette in the active editor's Stoat instance
        if let Some(editor) = self.active_editor() {
            let keymap = self.keymap.clone();
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_command_palette(&keymap, cx);
                });
            });
            cx.notify();
        }
    }

    /// Handle opening the buffer finder
    fn handle_open_buffer_finder(
        &mut self,
        _: &OpenBufferFinder,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Collect buffer IDs visible in all panes
        let visible_buffer_ids: Vec<text::BufferId> = self
            .pane_editors
            .values()
            .filter_map(|editor| editor.read(cx).stoat.read(cx).active_buffer_id(cx))
            .collect();

        // Open buffer finder in the active editor's Stoat instance
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_buffer_finder(&visible_buffer_ids, cx);
                });
            });
            cx.notify();
        }
    }

    /// Handle opening the git status modal
    fn handle_open_git_status(
        &mut self,
        _: &OpenGitStatus,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Open git status in the active editor's Stoat instance
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_git_status(cx);
                });
            });
            cx.notify();
        }
    }

    /// Handle opening the diff review mode
    fn handle_open_diff_review(
        &mut self,
        _: &OpenDiffReview,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Open diff review in the active editor's Stoat instance
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_diff_review(cx);
                });
            });
            cx.notify();
        }
    }

    /// Handle opening the help overlay or modal.
    ///
    /// This implements the double-? pattern:
    /// - First press: Show help overlay (non-modal)
    /// - Second press (while overlay visible): Open help modal (modal)
    fn handle_open_help_overlay(
        &mut self,
        _: &OpenHelpOverlay,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            "handle_open_help_overlay called, help_overlay_visible={}",
            self.help_overlay_visible
        );
        if self.help_overlay_visible {
            // Overlay already showing - open full modal
            debug!("Opening help modal");
            if let Some(editor) = self.active_editor() {
                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        stoat.open_help_modal(cx);
                    });
                });
            }
            self.help_overlay_visible = false;
        } else {
            // Show overlay
            debug!("Showing help overlay");
            self.help_overlay_visible = true;
        }
        cx.notify();
    }

    /// Handle opening the help modal directly.
    ///
    /// This is for command palette or programmatic access to help.
    fn handle_open_help_modal(
        &mut self,
        _: &OpenHelpModal,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_help_modal(cx);
                });
            });
        }
        self.help_overlay_visible = false;
        cx.notify();
    }

    /// Handle dismissing the help modal.
    fn handle_help_modal_dismiss(
        &mut self,
        _: &HelpModalDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.help_modal_dismiss(cx);
                });
            });
        }
        cx.notify();
    }

    /// Handle opening the about modal.
    fn handle_open_about_modal(
        &mut self,
        _: &OpenAboutModal,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_about_modal(cx);
                });
            });
        }
        cx.notify();
    }

    /// Handle dismissing the about modal.
    fn handle_about_modal_dismiss(
        &mut self,
        _: &AboutModalDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.about_modal_dismiss(cx);
                });
            });
        }
        cx.notify();
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
        self.pane_editors.insert(new_pane_id, new_editor);
        self.active_pane = new_pane_id;
    }

    /// Handle split up action
    fn handle_split_up(&mut self, _: &SplitUp, window: &mut Window, cx: &mut Context<'_, Self>) {
        debug!(
            active_pane = self.active_pane,
            direction = "Up",
            "Splitting pane"
        );

        // Create new Stoat that shares the buffer but has independent cursor/scroll state
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            cx.new(|cx| active_editor.read(cx).stoat.read(cx).clone_for_split())
        } else {
            cx.new(|cx| Stoat::new(stoat::Config::default(), cx))
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));

        // Set entity reference so EditorView can pass it to EditorElement
        new_editor.update(cx, |view, _| {
            view.set_entity(new_editor.clone());
        });

        self.split(SplitDirection::Up, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

        // Update minimap to show new active pane's content
        self.update_minimap_to_active_pane(cx);

        // Exit Pane mode after command
        self.exit_pane_mode(cx);

        cx.notify();
    }

    /// Handle split down action
    fn handle_split_down(
        &mut self,
        _: &SplitDown,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            active_pane = self.active_pane,
            direction = "Down",
            "Splitting pane"
        );

        // Create new Stoat that shares the buffer but has independent cursor/scroll state
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            cx.new(|cx| active_editor.read(cx).stoat.read(cx).clone_for_split())
        } else {
            cx.new(|cx| Stoat::new(stoat::Config::default(), cx))
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));

        // Set entity reference so EditorView can pass it to EditorElement
        new_editor.update(cx, |view, _| {
            view.set_entity(new_editor.clone());
        });

        self.split(SplitDirection::Down, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

        // Update minimap to show new active pane's content
        self.update_minimap_to_active_pane(cx);

        // Exit Pane mode after command
        self.exit_pane_mode(cx);

        cx.notify();
    }

    /// Handle split left action
    fn handle_split_left(
        &mut self,
        _: &SplitLeft,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            active_pane = self.active_pane,
            direction = "Left",
            "Splitting pane"
        );

        // Create new Stoat that shares the buffer but has independent cursor/scroll state
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            cx.new(|cx| active_editor.read(cx).stoat.read(cx).clone_for_split())
        } else {
            cx.new(|cx| Stoat::new(stoat::Config::default(), cx))
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));

        // Set entity reference so EditorView can pass it to EditorElement
        new_editor.update(cx, |view, _| {
            view.set_entity(new_editor.clone());
        });

        self.split(SplitDirection::Left, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

        // Update minimap to show new active pane's content
        self.update_minimap_to_active_pane(cx);

        // Exit Pane mode after command
        self.exit_pane_mode(cx);

        cx.notify();
    }

    /// Handle split right action
    fn handle_split_right(
        &mut self,
        _: &SplitRight,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            active_pane = self.active_pane,
            direction = "Right",
            "Splitting pane"
        );

        // Create new Stoat that shares the buffer but has independent cursor/scroll state
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            cx.new(|cx| active_editor.read(cx).stoat.read(cx).clone_for_split())
        } else {
            cx.new(|cx| Stoat::new(stoat::Config::default(), cx))
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));

        // Set entity reference so EditorView can pass it to EditorElement
        new_editor.update(cx, |view, _| {
            view.set_entity(new_editor.clone());
        });

        self.split(SplitDirection::Right, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

        // Update minimap to show new active pane's content
        self.update_minimap_to_active_pane(cx);

        // Exit Pane mode after command
        self.exit_pane_mode(cx);

        cx.notify();
    }

    /// Get the pane in the given direction (simplified tree-order navigation)
    fn get_pane_in_direction(&self, direction: SplitDirection) -> Option<PaneId> {
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

    /// Handle focus pane left action
    fn handle_focus_pane_left(
        &mut self,
        _: &FocusPaneLeft,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(new_pane) = self.get_pane_in_direction(SplitDirection::Left) {
            debug!(
                from_pane = self.active_pane,
                to_pane = new_pane,
                direction = "Left",
                "Focusing pane"
            );
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }

            // Update minimap to show new active pane's content
            self.update_minimap_to_active_pane(cx);

            // Exit Pane mode after command
            self.exit_pane_mode(cx);

            cx.notify();
        } else {
            debug!(
                current_pane = self.active_pane,
                direction = "Left",
                "No pane in direction"
            );
        }
    }

    /// Handle focus pane right action
    fn handle_focus_pane_right(
        &mut self,
        _: &FocusPaneRight,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(new_pane) = self.get_pane_in_direction(SplitDirection::Right) {
            debug!(
                from_pane = self.active_pane,
                to_pane = new_pane,
                direction = "Right",
                "Focusing pane"
            );
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }

            // Update minimap to show new active pane's content
            self.update_minimap_to_active_pane(cx);

            // Exit Pane mode after command
            self.exit_pane_mode(cx);

            cx.notify();
        } else {
            debug!(
                current_pane = self.active_pane,
                direction = "Right",
                "No pane in direction"
            );
        }
    }

    /// Handle focus pane up action
    fn handle_focus_pane_up(
        &mut self,
        _: &FocusPaneUp,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(new_pane) = self.get_pane_in_direction(SplitDirection::Up) {
            debug!(
                from_pane = self.active_pane,
                to_pane = new_pane,
                direction = "Up",
                "Focusing pane"
            );
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }

            // Update minimap to show new active pane's content
            self.update_minimap_to_active_pane(cx);

            // Exit Pane mode after command
            self.exit_pane_mode(cx);

            cx.notify();
        } else {
            debug!(
                current_pane = self.active_pane,
                direction = "Up",
                "No pane in direction"
            );
        }
    }

    /// Handle focus pane down action
    fn handle_focus_pane_down(
        &mut self,
        _: &FocusPaneDown,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(new_pane) = self.get_pane_in_direction(SplitDirection::Down) {
            debug!(
                from_pane = self.active_pane,
                to_pane = new_pane,
                direction = "Down",
                "Focusing pane"
            );
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }

            // Update minimap to show new active pane's content
            self.update_minimap_to_active_pane(cx);

            // Exit Pane mode after command
            self.exit_pane_mode(cx);

            cx.notify();
        } else {
            debug!(
                current_pane = self.active_pane,
                direction = "Down",
                "No pane in direction"
            );
        }
    }

    /// Handle close pane action
    fn handle_close_pane(
        &mut self,
        _: &ClosePane,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let pane_to_close = self.active_pane;

        debug!(pane_id = pane_to_close, "Attempting to close pane");

        // Try to remove the pane from the group
        match self.pane_group.remove(pane_to_close) {
            Ok(()) => {
                // Successfully removed - clean up editor and switch focus
                self.pane_editors.remove(&pane_to_close);

                // Get remaining panes and focus the first one
                let remaining_panes = self.pane_group.panes();
                if let Some(&new_active_pane) = remaining_panes.first() {
                    debug!(
                        closed_pane = pane_to_close,
                        new_active_pane,
                        remaining_count = remaining_panes.len(),
                        "Pane closed, switching focus"
                    );

                    self.active_pane = new_active_pane;
                    if let Some(editor) = self.pane_editors.get(&new_active_pane) {
                        window.focus(&editor.read(cx).focus_handle(cx));
                    }

                    // Update minimap to show new active pane's content
                    self.update_minimap_to_active_pane(cx);

                    // Exit Pane mode after command
                    self.exit_pane_mode(cx);

                    cx.notify();
                }
            },
            Err(e) => {
                debug!(
                    pane_id = pane_to_close,
                    error = %e,
                    "Cannot close pane"
                );
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

    /// Handle toggle minimap action
    ///
    /// Toggles between AlwaysVisible and AlwaysHidden
    fn handle_toggle_minimap(
        &mut self,
        _: &ToggleMinimap,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.minimap_visibility = match self.minimap_visibility {
            MinimapVisibility::AlwaysVisible => MinimapVisibility::AlwaysHidden,
            MinimapVisibility::AlwaysHidden | MinimapVisibility::ScrollHint { .. } => {
                MinimapVisibility::AlwaysVisible
            },
        };

        // Reset scroll tracking and fade state when changing modes
        self.last_editor_scroll_y = None;
        self.minimap_fade_state = MinimapFadeState::Hidden;

        debug!(
            minimap_visibility = ?self.minimap_visibility,
            "Toggled minimap visibility"
        );
        cx.notify();
    }

    /// Handle show minimap on scroll action
    ///
    /// Enables ScrollHint mode where minimap appears on large scrolls
    fn handle_show_minimap_on_scroll(
        &mut self,
        _: &ShowMinimapOnScroll,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.minimap_visibility = MinimapVisibility::ScrollHint {
            threshold_lines: SCROLL_HINT_DEFAULT_THRESHOLD,
        };

        // Reset scroll tracking and fade state when changing modes
        self.last_editor_scroll_y = None;
        self.minimap_fade_state = MinimapFadeState::Hidden;

        debug!("Enabled minimap scroll hint mode");
        cx.notify();
    }

    /// Recursively render a member of the pane tree.
    fn render_member(&self, member: &Member, basis: usize) -> AnyElement {
        match member {
            Member::Pane(pane_id) => {
                if let Some(editor) = self.pane_editors.get(pane_id) {
                    div()
                        .flex_1()
                        .size_full()
                        .child(editor.clone())
                        .into_any_element()
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
        // Track scroll position for ScrollHint mode
        // Extract early to avoid borrow conflicts with later code
        let current_scroll_y = self.pane_editors.get(&self.active_pane).map(|editor| {
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
            file_finder_data,
            command_palette_data,
            buffer_finder_data,
            git_status_data,
            status_bar_data,
            minimap_scroll_to_set,
            thumb_calculation_data,
        ) = self
            .pane_editors
            .get(&self.active_pane)
            .map(|editor| {
                let stoat_entity = editor.read(cx).stoat.clone();
                let stoat = stoat_entity.read(cx);
                let key_context = stoat.key_context();
                let mode_name = stoat.mode();
                let display = stoat
                    .get_mode(mode_name)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_else(|| mode_name.to_uppercase());

                // Extract file finder data if in FileFinder context
                let ff_data = if key_context == KeyContext::FileFinder {
                    let query = stoat
                        .file_finder_input()
                        .map(|buffer| {
                            let buffer_snapshot = buffer.read(cx).snapshot();
                            buffer_snapshot.text()
                        })
                        .unwrap_or_default();
                    Some((
                        query,
                        stoat.file_finder_filtered().to_vec(),
                        stoat.file_finder_selected(),
                        stoat.file_finder_preview().cloned(),
                    ))
                } else {
                    None
                };

                // Extract command palette data if in CommandPalette context
                let cp_data = if key_context == KeyContext::CommandPalette {
                    let query = stoat
                        .command_palette_input()
                        .map(|buffer| {
                            let buffer_snapshot = buffer.read(cx).snapshot();
                            buffer_snapshot.text()
                        })
                        .unwrap_or_default();
                    Some((
                        query,
                        stoat.command_palette_filtered().to_vec(),
                        stoat.command_palette_selected(),
                    ))
                } else {
                    None
                };

                // Extract buffer finder data if in BufferFinder context
                let bf_data = if key_context == KeyContext::BufferFinder {
                    let query = stoat
                        .buffer_finder_input()
                        .map(|buffer| {
                            let buffer_snapshot = buffer.read(cx).snapshot();
                            buffer_snapshot.text()
                        })
                        .unwrap_or_default();
                    Some((
                        query,
                        stoat.buffer_finder_filtered().to_vec(),
                        stoat.buffer_finder_selected(),
                    ))
                } else {
                    None
                };

                // Extract git status data if in Git context
                let gs_data = if key_context == KeyContext::Git {
                    Some((
                        stoat.git_status_files().to_vec(),
                        stoat.git_status_filtered().to_vec(),
                        stoat.git_status_filter(),
                        stoat.git_status_files().len(),
                        stoat.git_status_selected(),
                        stoat.git_status_preview().cloned(),
                        stoat.git_status_branch_info().cloned(),
                    ))
                } else {
                    None
                };

                // Extract status bar data
                let sb_data = (
                    display.clone(),
                    stoat.git_status_branch_info().cloned(),
                    stoat.git_status_files().to_vec(),
                    stoat.current_file_path().map(|p| p.display().to_string()),
                    stoat.diff_review_progress(),
                    stoat.diff_review_file_progress(),
                    stoat.diff_review_hunk_position(cx),
                    // Only show comparison mode when in diff_review mode
                    if mode_name == "diff_review" {
                        Some(stoat.diff_comparison_mode())
                    } else {
                        None
                    },
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
        let bindings = crate::keymap_query::bindings_for_mode(&self.keymap, &active_mode);

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
                    .on_action(cx.listener(Self::handle_split_up))
                    .on_action(cx.listener(Self::handle_split_down))
                    .on_action(cx.listener(Self::handle_split_left))
                    .on_action(cx.listener(Self::handle_split_right))
                    .on_action(cx.listener(Self::handle_close_pane))
                    .on_action(cx.listener(Self::handle_focus_pane_up))
                    .on_action(cx.listener(Self::handle_focus_pane_down))
                    .on_action(cx.listener(Self::handle_focus_pane_left))
                    .on_action(cx.listener(Self::handle_focus_pane_right))
                    .on_action(cx.listener(Self::handle_open_file_finder))
                    .on_action(cx.listener(Self::handle_open_command_palette))
                    .on_action(cx.listener(Self::handle_open_buffer_finder))
                    .on_action(cx.listener(Self::handle_open_git_status))
                    .on_action(cx.listener(Self::handle_open_diff_review))
                    .on_action(cx.listener(Self::handle_open_help_overlay))
                    .on_action(cx.listener(Self::handle_open_help_modal))
                    .on_action(cx.listener(Self::handle_help_modal_dismiss))
                    .on_action(cx.listener(Self::handle_open_about_modal))
                    .on_action(cx.listener(Self::handle_about_modal_dismiss))
                    .on_action(cx.listener(Self::handle_toggle_minimap))
                    .on_action(cx.listener(Self::handle_show_minimap_on_scroll))
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
                    comparison_mode,
                )| {
                    div.child(StatusBar::new(
                        mode,
                        branch,
                        files,
                        path,
                        review_progress,
                        review_file_progress,
                        hunk_position,
                        comparison_mode,
                    ))
                },
            )
    }
}
