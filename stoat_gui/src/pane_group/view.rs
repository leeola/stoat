use crate::{
    command_overlay::CommandOverlay, editor::view::EditorView, file_finder::FileFinder,
    pane_group::element::pane_axis,
};
use gpui::{
    div, prelude::FluentBuilder, AnyElement, App, AppContext, Context, DismissEvent, Entity,
    FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Subscription, Window,
};
use std::{collections::HashMap, path::PathBuf, rc::Rc};
use stoat::{
    actions::{
        ClosePane, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp, OpenFileFinder,
        SplitDown, SplitLeft, SplitRight, SplitUp,
    },
    pane::{Member, PaneAxis, PaneGroup, PaneId, SplitDirection},
    Stoat,
};
use tracing::debug;

/// Main view that manages multiple editor panes in a tree layout.
///
/// PaneGroupView wraps a [`PaneGroup`] (from stoat core) and maintains
/// the mapping from [`PaneId`] to [`EditorView`] entities. It handles
/// split operations, pane focus, and recursive rendering of the pane tree.
pub struct PaneGroupView {
    pane_group: PaneGroup,
    pane_editors: HashMap<PaneId, Entity<EditorView>>,
    active_pane: PaneId,
    focus_handle: FocusHandle,
    keymap: Rc<gpui::Keymap>,
    file_finder: Option<Entity<FileFinder>>,
    _file_finder_subscription: Option<Subscription>,
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

        Self {
            pane_group,
            pane_editors,
            active_pane: initial_pane_id,
            focus_handle: cx.focus_handle(),
            keymap,
            file_finder: None,
            _file_finder_subscription: None,
        }
    }

    /// Get the active editor view
    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.pane_editors.get(&self.active_pane)
    }

    /// Exit Pane mode if currently in it, returning to Normal mode.
    ///
    /// This is called after pane commands execute to make Pane mode a one-shot mode.
    fn exit_pane_mode(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(editor) = self.pane_editors.get_mut(&self.active_pane) {
            editor.update(cx, |editor, cx| {
                if editor.stoat().mode() == "pane" {
                    editor.stoat_mut().set_mode("normal");
                    cx.notify();
                }
            });
        }
    }

    /// Handle opening the file finder
    fn handle_open_file_finder(
        &mut self,
        _: &OpenFileFinder,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // For now, use hardcoded test files
        // TODO: Replace with actual file discovery
        let test_files = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("Cargo.toml"),
            PathBuf::from("README.md"),
        ];

        // Get the current focus to restore later
        let previous_focus = self
            .active_editor()
            .map(|editor| editor.read(cx).focus_handle(cx));

        let file_finder = cx.new(|cx| FileFinder::new(test_files, previous_focus, window, cx));

        // Subscribe to dismiss event to close the finder
        self._file_finder_subscription = Some(cx.subscribe(
            &file_finder,
            |this, _finder, _event: &DismissEvent, cx| {
                this.file_finder = None;
                this._file_finder_subscription = None;
                cx.notify();
            },
        ));

        self.file_finder = Some(file_finder.clone());
        window.focus(&file_finder.read(cx).focus_handle(cx));
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

        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Up, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

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

        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Down, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

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

        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Left, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

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

        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Right, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));

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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Get the mode from the active editor
        let (active_mode, mode_display) = self
            .pane_editors
            .get(&self.active_pane)
            .map(|editor| {
                let stoat = editor.read(cx).stoat();
                let mode_name = stoat.mode();
                let display = stoat
                    .get_mode(mode_name)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_else(|| mode_name.to_uppercase());
                (mode_name, display)
            })
            .unwrap_or(("normal", "NORMAL".to_string()));

        // Query keymap for bindings in the current mode
        let bindings = crate::keymap_query::bindings_for_mode(&self.keymap, active_mode);

        div()
            .size_full()
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
            .child(self.render_member(self.pane_group.root(), 0))
            .child(CommandOverlay::new(mode_display, bindings))
            .when_some(self.file_finder.clone(), |div, finder| div.child(finder))
    }
}
