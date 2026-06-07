//! Modal that asks the user to approve, defer, or deny a Claude
//! tool call when an `always_confirm` rule matches.
//!
//! A permission policy sends a
//! [`crate::host::PermissionPrompt`] over an mpsc channel. The
//! [`crate::Stoat`] event loop receives the prompt, builds an
//! [`ApprovalModal`] holding the prompt's `oneshot::Sender`, and
//! routes keys to it until the user picks a button. The captured
//! sender is consumed exactly once when the user decides; the
//! awaiting policy task converts the [`ApprovalDecision`] into a
//! [`crate::host::PermissionResult`].

use crate::host::{ApprovalDecision, PermissionPrompt};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

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

const DEFAULT_FOCUS: usize = 1;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Button {
    pub label: &'static str,
    pub decision: ApprovalDecision,
}

pub(crate) struct ApprovalModal {
    tool: String,
    input: String,
    focused: usize,
    response_tx: Option<oneshot::Sender<ApprovalDecision>>,
}

pub(crate) enum ModalOutcome {
    None,
    #[allow(dead_code, reason = "decision is observed via the oneshot in tests")]
    Decided(ApprovalDecision),
}

impl ApprovalModal {
    pub fn new(prompt: PermissionPrompt) -> Self {
        Self {
            tool: prompt.tool,
            input: prompt.input,
            focused: DEFAULT_FOCUS,
            response_tx: Some(prompt.response_tx),
        }
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn focused_button(&self) -> usize {
        self.focused
    }

    pub fn buttons() -> &'static [Button] {
        &BUTTONS
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModalOutcome {
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.focused = (self.focused + 1) % BUTTONS.len();
                ModalOutcome::None
            },
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.focused = (self.focused + BUTTONS.len() - 1) % BUTTONS.len();
                ModalOutcome::None
            },
            KeyCode::Enter => self.decide(BUTTONS[self.focused].decision),
            KeyCode::Esc => self.decide(ApprovalDecision::Deny),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.decide(ApprovalDecision::Deny)
            },
            _ => ModalOutcome::None,
        }
    }

    fn decide(&mut self, decision: ApprovalDecision) -> ModalOutcome {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(decision);
        }
        ModalOutcome::Decided(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::keys;

    fn modal_with_decision_capture() -> (ApprovalModal, oneshot::Receiver<ApprovalDecision>) {
        let (tx, rx) = oneshot::channel();
        let modal = ApprovalModal::new(PermissionPrompt {
            tool: "Bash".to_string(),
            input: r#"{"command": "cargo install ripgrep"}"#.to_string(),
            response_tx: tx,
        });
        (modal, rx)
    }

    #[test]
    fn focus_starts_on_allow_once() {
        let (modal, _rx) = modal_with_decision_capture();
        assert_eq!(modal.focused_button(), DEFAULT_FOCUS);
        assert_eq!(BUTTONS[DEFAULT_FOCUS].label, "Allow-once");
    }

    #[test]
    fn tab_cycles_forward() {
        let (mut modal, _rx) = modal_with_decision_capture();
        modal.handle_key(keys::key(KeyCode::Tab));
        assert_eq!(modal.focused_button(), 2);
        modal.handle_key(keys::key(KeyCode::Tab));
        assert_eq!(modal.focused_button(), 3);
        modal.handle_key(keys::key(KeyCode::Tab));
        assert_eq!(modal.focused_button(), 0);
    }

    #[test]
    fn back_tab_cycles_backward() {
        let (mut modal, _rx) = modal_with_decision_capture();
        modal.handle_key(keys::key(KeyCode::BackTab));
        assert_eq!(modal.focused_button(), 0);
        modal.handle_key(keys::key(KeyCode::BackTab));
        assert_eq!(modal.focused_button(), 3);
    }

    #[test]
    fn enter_picks_focused_decision() {
        let (mut modal, rx) = modal_with_decision_capture();
        modal.handle_key(keys::key(KeyCode::Tab));
        let outcome = modal.handle_key(keys::key(KeyCode::Enter));
        match outcome {
            ModalOutcome::Decided(ApprovalDecision::AlwaysAllow) => {},
            _ => panic!("expected AlwaysAllow"),
        }
        assert_eq!(rx.blocking_recv(), Ok(ApprovalDecision::AlwaysAllow));
    }

    #[test]
    fn esc_picks_deny() {
        let (mut modal, rx) = modal_with_decision_capture();
        let outcome = modal.handle_key(keys::key(KeyCode::Esc));
        match outcome {
            ModalOutcome::Decided(ApprovalDecision::Deny) => {},
            _ => panic!("expected Deny"),
        }
        assert_eq!(rx.blocking_recv(), Ok(ApprovalDecision::Deny));
    }

    #[test]
    fn ctrl_c_picks_deny() {
        let (mut modal, rx) = modal_with_decision_capture();
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        modal.handle_key(event);
        assert_eq!(rx.blocking_recv(), Ok(ApprovalDecision::Deny));
    }

    #[test]
    fn other_keys_keep_modal_open() {
        let (mut modal, _rx) = modal_with_decision_capture();
        let outcome = modal.handle_key(keys::key(KeyCode::Char('z')));
        assert!(matches!(outcome, ModalOutcome::None));
    }

    #[test]
    fn enter_after_focus_on_each_button_returns_correct_decision() {
        for (idx, expected) in BUTTONS.iter().map(|b| b.decision).enumerate() {
            let (mut modal, rx) = modal_with_decision_capture();
            let steps = (idx + BUTTONS.len() - DEFAULT_FOCUS) % BUTTONS.len();
            for _ in 0..steps {
                modal.handle_key(keys::key(KeyCode::Tab));
            }
            modal.handle_key(keys::key(KeyCode::Enter));
            assert_eq!(rx.blocking_recv(), Ok(expected));
        }
    }

    #[test]
    fn snapshot_permission_prompt_modal() {
        let mut h = crate::Stoat::test();
        let (tx, _rx) = oneshot::channel();
        let prompt = PermissionPrompt {
            tool: "Bash".to_string(),
            input: r#"{"command": "cargo install ripgrep"}"#.to_string(),
            response_tx: tx,
        };
        h.stoat.enqueue_permission_prompt(prompt);
        h.assert_snapshot("permission_prompt_modal");
    }
}
