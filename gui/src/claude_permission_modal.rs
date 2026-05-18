//! Claude tool-permission approval modal.
//!
//! Renders the tool name + raw input from a
//! [`stoat::host::PermissionPrompt`] above four buttons (`Allow`,
//! `Allow-once`, `Always-allow`, `Deny`) and resolves the captured
//! `oneshot::Sender<ApprovalDecision>` once the user picks one.
//! Mirrors the TUI's `ApprovalModal`
//! (`stoat/src/permission_prompt.rs:44`).
//!
//! Dismiss semantics: explicit button choice resolves with the
//! matching [`ApprovalDecision`]; Escape, backdrop click, or any
//! other dismiss path resolves with [`ApprovalDecision::Deny`] via
//! [`ModalView::on_before_dismiss`]. The single `decide` funnel
//! guarantees the sender is consumed exactly once.

use crate::{modal_layer::ModalView, workspace::Workspace};
use gpui::{
    div, AnyElement, App, Context, DismissEvent, ElementId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use stoat::host::{ApprovalDecision, PermissionPrompt};
use stoat_action::ActionKind;

/// Layout-time button definition. Click maps to the captured
/// decision; ordering mirrors the TUI's `BUTTONS` constant.
#[derive(Debug, Clone, Copy)]
struct Button {
    label: &'static str,
    decision: ApprovalDecision,
}

const BUTTONS: [Button; 4] = [
    Button {
        label: "Allow",
        decision: ApprovalDecision::Allow,
    },
    Button {
        label: "Allow-once",
        decision: ApprovalDecision::AllowOnce,
    },
    Button {
        label: "Always-allow",
        decision: ApprovalDecision::AlwaysAllow,
    },
    Button {
        label: "Deny",
        decision: ApprovalDecision::Deny,
    },
];

/// Default focused button on open. Matches the TUI's
/// `DEFAULT_FOCUS` so muscle memory carries across surfaces.
const DEFAULT_FOCUS: usize = 1;

pub struct PermissionModal {
    tool: String,
    input: String,
    focused: usize,
    response_tx: Option<tokio::sync::oneshot::Sender<ApprovalDecision>>,
    focus_handle: FocusHandle,
    #[allow(dead_code)]
    workspace: WeakEntity<Workspace>,
}

impl PermissionModal {
    pub fn new(
        prompt: PermissionPrompt,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self {
            tool: prompt.tool,
            input: prompt.input,
            focused: DEFAULT_FOCUS,
            response_tx: Some(prompt.response_tx),
            focus_handle: cx.focus_handle(),
            workspace,
        }
    }

    #[cfg(test)]
    pub(crate) fn tool(&self) -> &str {
        &self.tool
    }

    #[cfg(test)]
    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    #[cfg(test)]
    pub(crate) fn focused_button(&self) -> usize {
        self.focused
    }

    /// Test-only check that the sender has been consumed. Used by
    /// tests to assert that subsequent dismiss paths do not double-
    /// resolve a prompt that already decided.
    #[cfg(test)]
    pub(crate) fn response_pending(&self) -> bool {
        self.response_tx.is_some()
    }

    fn decide(&mut self, decision: ApprovalDecision, cx: &mut Context<'_, Self>) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(decision);
        }
        cx.emit(DismissEvent);
    }
}

impl Render for PermissionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let header = div()
            .px_2()
            .py_1()
            .child(SharedString::from(format!("Tool: {}", self.tool)));
        let input_label = div().px_2().child(SharedString::from("Input:"));
        let input_body = div()
            .px_2()
            .py_1()
            .child(SharedString::from(self.input.clone()));

        let mut buttons_row = div().flex().flex_row().px_2().py_1();
        for (idx, button) in BUTTONS.iter().enumerate() {
            buttons_row = buttons_row.child(render_button(*button, idx, self.focused, cx));
        }

        div()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            .child(header)
            .child(input_label)
            .child(input_body)
            .child(buttons_row)
    }
}

fn render_button(
    button: Button,
    idx: usize,
    focused: usize,
    cx: &mut Context<'_, PermissionModal>,
) -> AnyElement {
    let element_id: ElementId = SharedString::from(format!("claude_perm_btn:{idx}")).into();
    let focus_marker = if idx == focused { "*" } else { " " };
    let label = SharedString::from(format!("{focus_marker} {} ", button.label));
    let decision = button.decision;
    div()
        .id(element_id)
        .px_2()
        .child(label)
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.decide(decision, cx);
        }))
        .into_any_element()
}

impl Focusable for PermissionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for PermissionModal {}

impl ModalView for PermissionModal {
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> bool {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(ApprovalDecision::Deny);
        }
        true
    }

    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if action.kind() == ActionKind::DismissModal {
            self.decide(ApprovalDecision::Deny, cx);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{globals::ExecutorGlobal, workspace::Workspace};
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};
    use tokio::sync::oneshot;

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn fresh_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        install_executor(cx);
        cx.add_window_view(|_window, cx| {
            Workspace::new("perm-test", PathBuf::from("/tmp/perm"), cx)
        })
    }

    fn new_prompt(
        tool: &str,
        input: &str,
    ) -> (PermissionPrompt, oneshot::Receiver<ApprovalDecision>) {
        let (tx, rx) = oneshot::channel();
        let prompt = PermissionPrompt {
            tool: tool.to_string(),
            input: input.to_string(),
            response_tx: tx,
        };
        (prompt, rx)
    }

    fn build_modal(
        vcx: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        prompt: PermissionPrompt,
    ) -> Entity<PermissionModal> {
        let weak = workspace.downgrade();
        vcx.update(|_window, cx| cx.new(|cx| PermissionModal::new(prompt, weak, cx)))
    }

    #[test]
    fn renders_tool_name_and_input_metadata() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, _rx) = new_prompt("Bash", "{\"command\":\"ls\"}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.read_with(vcx, |m, _| {
            assert_eq!(m.tool(), "Bash");
            assert_eq!(m.input(), "{\"command\":\"ls\"}");
            assert_eq!(m.focused_button(), DEFAULT_FOCUS);
        });
    }

    #[test]
    fn clicking_allow_button_sends_allow_decision() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{\"command\":\"ls\"}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update(vcx, |m, cx| m.decide(ApprovalDecision::Allow, cx));
        vcx.run_until_parked();

        let decision = rx.blocking_recv().expect("decision delivered");
        assert_eq!(decision, ApprovalDecision::Allow);
    }

    #[test]
    fn clicking_allow_once_sends_allow_once_decision() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update(vcx, |m, cx| m.decide(ApprovalDecision::AllowOnce, cx));
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::AllowOnce
        );
    }

    #[test]
    fn clicking_always_allow_sends_always_allow_decision() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update(vcx, |m, cx| m.decide(ApprovalDecision::AlwaysAllow, cx));
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::AlwaysAllow
        );
    }

    #[test]
    fn clicking_deny_sends_deny_decision() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update(vcx, |m, cx| m.decide(ApprovalDecision::Deny, cx));
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn on_before_dismiss_without_choice_sends_deny() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update_in(vcx, |m, window, cx| {
            let proceed = m.on_before_dismiss(window, cx);
            assert!(proceed, "modal should allow dismissal");
        });
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn on_before_dismiss_after_decision_is_idempotent() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update(vcx, |m, cx| m.decide(ApprovalDecision::Allow, cx));
        modal.update_in(vcx, |m, window, cx| {
            let proceed = m.on_before_dismiss(window, cx);
            assert!(proceed);
            assert!(
                !m.response_pending(),
                "sender consumed by the explicit Allow decision",
            );
        });
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn dismiss_modal_action_sends_deny() {
        let mut cx = TestAppContext::single();
        let (workspace, vcx) = fresh_workspace(&mut cx);
        let (prompt, rx) = new_prompt("Bash", "{}");
        let modal = build_modal(vcx, &workspace, prompt);
        modal.update_in(vcx, |m, window, cx| {
            let consumed = m.handle_action(&stoat_action::DismissModal, window, cx);
            assert!(consumed);
        });
        assert_eq!(
            rx.blocking_recv().expect("decision delivered"),
            ApprovalDecision::Deny
        );
    }
}
