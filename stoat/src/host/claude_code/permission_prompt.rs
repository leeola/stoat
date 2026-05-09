//! Approval-prompt protocol between an async [`PermissionCallback`]
//! and the synchronous UI thread.
//!
//! When a permission policy needs the user's input (e.g. a tool call
//! matched an `always_confirm` rule), it constructs a
//! [`PermissionPrompt`] with a fresh `oneshot::Sender`, ships the
//! prompt to the UI thread over an mpsc channel, and awaits the
//! response on the matching `oneshot::Receiver`. The UI thread
//! renders the approval modal, captures the user's choice as an
//! [`ApprovalDecision`], and sends it back through the captured
//! sender. The async callback resumes and translates the decision
//! into a [`super::permission::PermissionResult`].

use tokio::sync::oneshot;

/// User-selected outcome for an interactive permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow this call and remember the exact `(tool, input)` pair
    /// for the rest of the process lifetime.
    Allow,
    /// Allow this call only.
    AllowOnce,
    /// Allow this call and append a literal-match regex to the
    /// runtime always_allow list for the tool. v1 only persists
    /// for the process lifetime; future work adds disk-backed
    /// stcfg storage.
    AlwaysAllow,
    /// Deny this call. No session state changes; future identical
    /// calls re-prompt.
    Deny,
}

/// One pending interactive permission prompt.
///
/// Constructed on the policy side; consumed on the UI side. The
/// `response_tx` is moved into the modal and consumed exactly
/// once when the user picks a decision.
pub struct PermissionPrompt {
    pub tool: String,
    pub input: String,
    pub response_tx: oneshot::Sender<ApprovalDecision>,
}
