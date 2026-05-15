use crate::{item::ItemHandle, status_bar::StatusItemView, theme::statusbar_text_color};
use gpui::{
    div, Context, FontWeight, IntoElement, ParentElement, Render, SharedString, Styled, Window,
};

/// Status-bar item that surfaces the workspace name. The name is
/// pushed in from [`Workspace::set_name`] rather than pulled, so
/// the label holds only the current value and does not back-
/// reference the workspace.
///
/// Renders the name in bold over the focused status-bar text
/// color so it reads as the surface's primary chrome label.
///
/// [`Workspace::set_name`]: crate::workspace::Workspace::set_name
pub struct WorkspaceLabel {
    name: SharedString,
}

impl WorkspaceLabel {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self { name: name.into() }
    }

    /// Replace the displayed name and re-render. A no-op when the
    /// new value matches the current name -- avoids spurious
    /// re-renders for callers that push unconditionally.
    pub fn set_name(&mut self, name: impl Into<SharedString>, cx: &mut Context<'_, Self>) {
        let name = name.into();
        if self.name == name {
            return;
        }
        self.name = name;
        cx.notify();
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }
}

impl Render for WorkspaceLabel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .px_2()
            .font_weight(FontWeight::BOLD)
            .text_color(statusbar_text_color(cx))
            .child(self.name.clone())
    }
}

impl StatusItemView for WorkspaceLabel {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _cx: &mut Context<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[test]
    fn new_stores_given_name() {
        let cx = TestAppContext::single();
        let label = cx.update(|cx| cx.new(|_| WorkspaceLabel::new("alpha")));
        label.read_with(&cx, |l, _| {
            assert_eq!(l.name(), &SharedString::from("alpha"));
        });
    }

    #[test]
    fn set_name_with_equal_value_is_noop() {
        let mut cx = TestAppContext::single();
        let label = cx.update(|cx| cx.new(|_| WorkspaceLabel::new("alpha")));
        label.update(&mut cx, |l, cx| l.set_name("alpha", cx));
        label.read_with(&cx, |l, _| {
            assert_eq!(l.name(), &SharedString::from("alpha"));
        });
    }

    #[test]
    fn set_name_with_new_value_updates_field() {
        let mut cx = TestAppContext::single();
        let label = cx.update(|cx| cx.new(|_| WorkspaceLabel::new("alpha")));
        label.update(&mut cx, |l, cx| l.set_name("beta", cx));
        label.read_with(&cx, |l, _| {
            assert_eq!(l.name(), &SharedString::from("beta"));
        });
    }

    #[test]
    fn set_active_pane_item_is_noop() {
        let cx = TestAppContext::single();
        let label = cx.update(|cx| cx.new(|_| WorkspaceLabel::new("alpha")));
        cx.update(|cx| {
            label.update(cx, |l, cx| l.set_active_pane_item(None, cx));
        });
        label.read_with(&cx, |l, _| {
            assert_eq!(l.name(), &SharedString::from("alpha"));
        });
    }

    #[test]
    fn render_emits_current_name() {
        let mut cx = TestAppContext::single();
        let (label, vcx) = cx.add_window_view(|_, _| WorkspaceLabel::new("alpha"));
        vcx.run_until_parked();
        label.update(vcx, |l, cx| l.set_name("renamed", cx));
        vcx.run_until_parked();
        label.read_with(vcx, |l, _| {
            assert_eq!(l.name(), &SharedString::from("renamed"));
        });
    }
}
