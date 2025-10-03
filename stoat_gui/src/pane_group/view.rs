use crate::editor::view::EditorView;
use gpui::{
    div, AnyElement, App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window,
};
use std::collections::HashMap;
use stoat::pane::{Axis, Member, PaneAxis, PaneGroup, PaneId, SplitDirection};

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
        pane_editors.insert(initial_pane_id, initial_editor);

        Self {
            pane_group,
            pane_editors,
            active_pane: initial_pane_id,
            focus_handle: cx.focus_handle(),
        }
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

        for (_idx, member) in axis.members.iter().enumerate() {
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.render_member(self.pane_group.root()))
    }
}
