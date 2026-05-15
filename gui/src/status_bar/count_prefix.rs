use crate::{
    input_state_machine::InputStateMachine, item::ItemHandle, status_bar::StatusItemView,
    theme::statusbar_text_color,
};
use gpui::{
    div, AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window,
};

/// Status-bar item that surfaces the
/// [`InputStateMachine::pending_count`] prefix during count-active
/// modes. Renders nothing when the slot is `None`; renders bold
/// ` {n} ` over [`crate::theme::statusbar_text_color`] when set.
///
/// Subscribes to [`InputStateMachine`] notifications via
/// `cx.observe`, mirroring [`crate::status_bar::mode_badge::ModeBadge`]
/// -- both items react to the same state machine and the count is
/// always pushed alongside `cx.notify()` from every mutation site.
pub struct CountPrefix {
    input_state_machine: Entity<InputStateMachine>,
    _subscription: Subscription,
}

impl CountPrefix {
    pub fn new(input_state_machine: Entity<InputStateMachine>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.observe(&input_state_machine, |_, _, cx| cx.notify());
        Self {
            input_state_machine,
            _subscription: subscription,
        }
    }
}

impl Render for CountPrefix {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label: Option<AnyElement> =
            self.input_state_machine
                .read(cx)
                .pending_count()
                .map(|count| {
                    div()
                        .px_2()
                        .font_weight(FontWeight::BOLD)
                        .text_color(statusbar_text_color(cx))
                        .child(SharedString::from(format!(" {count} ")))
                        .into_any_element()
                });
        div().children(label)
    }
}

impl StatusItemView for CountPrefix {
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
    use crate::workspace::Workspace;
    use gpui::{AppContext, TestAppContext};
    use std::path::PathBuf;

    fn new_state_machine(cx: &mut TestAppContext) -> Entity<InputStateMachine> {
        let workspace =
            cx.update(|cx| cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx)));
        workspace.read_with(cx, |w, _| w.input_state_machine().clone())
    }

    fn new_count_prefix(
        cx: &mut TestAppContext,
        sm: Entity<InputStateMachine>,
    ) -> Entity<CountPrefix> {
        cx.update(|cx| cx.new(|cx| CountPrefix::new(sm, cx)))
    }

    #[test]
    fn new_subscribes_to_state_machine_notifications() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let item = new_count_prefix(&mut cx, sm.clone());
        item.read_with(&cx, |i, cx| {
            assert_eq!(i.input_state_machine.read(cx).pending_count(), None);
        });
    }

    #[test]
    fn pending_count_change_reflects_through_state_machine() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let (_item, vcx) = cx.add_window_view(|_, cx| CountPrefix::new(sm.clone(), cx));
        vcx.run_until_parked();

        sm.update(vcx, |sm, cx| sm.set_pending_count_for_test(Some(42), cx));
        vcx.run_until_parked();
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), Some(42)));

        sm.update(vcx, |sm, cx| sm.set_pending_count_for_test(None, cx));
        vcx.run_until_parked();
        sm.read_with(vcx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn set_active_pane_item_is_noop() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let item = new_count_prefix(&mut cx, sm.clone());
        cx.update(|cx| {
            item.update(cx, |i, cx| i.set_active_pane_item(None, cx));
        });
        item.read_with(&cx, |i, cx| {
            assert_eq!(i.input_state_machine.read(cx).pending_count(), None);
        });
    }
}
