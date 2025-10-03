use crate::editor::view::EditorView;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Window, div, rgb,
};
use std::collections::HashMap;
use stoat::{
    Stoat,
    actions::{
        FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp, SplitDown, SplitLeft,
        SplitRight, SplitUp,
    },
    pane::{Axis, Member, PaneAxis, PaneGroup, PaneId, SplitDirection},
};

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
}

impl PaneGroupView {
    /// Create a new pane group view with an initial editor entity.
    ///
    /// The caller must create the initial EditorView entity using App context.
    pub fn new(initial_editor: Entity<EditorView>, cx: &mut Context<'_, Self>) -> Self {
        let pane_group = PaneGroup::new();
        let initial_pane_id = pane_group.panes()[0];

        let mut pane_editors = HashMap::new();
        pane_editors.insert(initial_pane_id, initial_editor.clone());

        Self {
            pane_group,
            pane_editors,
            active_pane: initial_pane_id,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Get the active editor view
    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.pane_editors.get(&self.active_pane)
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
        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Up, new_editor.clone(), cx);
        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// Handle split down action
    fn handle_split_down(
        &mut self,
        _: &SplitDown,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Down, new_editor.clone(), cx);
        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// Handle split left action
    fn handle_split_left(
        &mut self,
        _: &SplitLeft,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Left, new_editor.clone(), cx);
        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// Handle split right action
    fn handle_split_right(
        &mut self,
        _: &SplitRight,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Clone the Stoat from the active pane so the new split shows the same buffer
        let new_stoat = if let Some(active_editor) = self.pane_editors.get(&self.active_pane) {
            active_editor.read(cx).stoat().clone()
        } else {
            Stoat::new(cx)
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        self.split(SplitDirection::Right, new_editor.clone(), cx);
        // Focus the newly created editor
        window.focus(&new_editor.read(cx).focus_handle(cx));
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
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }
            cx.notify();
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
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }
            cx.notify();
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
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }
            cx.notify();
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
            self.active_pane = new_pane;
            if let Some(editor) = self.pane_editors.get(&new_pane) {
                window.focus(&editor.read(cx).focus_handle(cx));
            }
            cx.notify();
        }
    }

    /// Recursively render a member of the pane tree.
    fn render_member(&self, member: &Member) -> AnyElement {
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
            Member::Axis(axis) => self.render_axis(axis),
        }
    }

    /// Render an axis with its children.
    fn render_axis(&self, axis: &PaneAxis) -> AnyElement {
        let mut container = div().size_full();

        container = match axis.axis {
            Axis::Horizontal => container.flex().flex_row(),
            Axis::Vertical => container.flex().flex_col(),
        };

        for (idx, member) in axis.members.iter().enumerate() {
            // Add divider before each child except the first
            if idx > 0 {
                let divider = match axis.axis {
                    Axis::Horizontal => {
                        // Vertical divider for side-by-side panes
                        div().w_px().h_full().bg(rgb(0x3c3c3c))
                    },
                    Axis::Vertical => {
                        // Horizontal divider for stacked panes
                        div().w_full().h_px().bg(rgb(0x3c3c3c))
                    },
                };
                container = container.child(divider);
            }

            // TODO: Use axis.flexes[idx] for custom sizing when PaneAxisElement is implemented
            let child = self.render_member(member);

            container = container.child(div().flex_1().size_full().child(child));
        }

        container.into_any_element()
    }
}

impl Focusable for PaneGroupView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaneGroupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_split_up))
            .on_action(cx.listener(Self::handle_split_down))
            .on_action(cx.listener(Self::handle_split_left))
            .on_action(cx.listener(Self::handle_split_right))
            .on_action(cx.listener(Self::handle_focus_pane_up))
            .on_action(cx.listener(Self::handle_focus_pane_down))
            .on_action(cx.listener(Self::handle_focus_pane_left))
            .on_action(cx.listener(Self::handle_focus_pane_right))
            .child(self.render_member(self.pane_group.root()))
    }
}
