//! Claude Code shared types: the [`AgentMessage`] union the app
//! consumes, plus the supporting event, plan/usage, permission, and
//! hook type interfaces.
//!
//! Data types are split into submodules:
//! - [`types`]: shared shape types (tool classification, plans, usage).
//! - [`events`]: session/task/hook lifecycle event enums.
//! - [`message`]: the [`AgentMessage`] union.
//! - [`hooks`]: hook-callback interface.
//! - [`permission`]: permission-callback interface and outcome types.
//! - [`denial`]: hardcoded denial policy.

mod chain;
mod denial;
mod events;
mod hooks;
mod message;
mod permission;
mod permission_prompt;
mod shell_chain;
mod types;

pub use chain::ChainedPermissionPolicy;
pub use denial::BashDenialPolicy;
pub use events::{HookLifecycleEvent, SessionStateEvent, TaskEvent};
pub use hooks::{HookCallback, HookDecision, HookEvent, HookKind, HookResponse};
pub use message::AgentMessage;
pub use permission::{
    PermissionBehavior, PermissionCallback, PermissionDestination, PermissionResult,
    PermissionRule, PermissionScope, PermissionSuggestion, ToolPermissionContext,
};
pub use permission_prompt::{ApprovalDecision, PermissionPrompt};
pub use types::{
    ModeInfo, ModelInfo, PlanEntry, PlanEntryStatus, TerminalMeta, TokenUsage, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_kind_round_trips_known_names() {
        let names = [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "SubagentStop",
            "SessionStart",
            "SessionEnd",
            "Notification",
            "PreCompact",
        ];
        for name in names {
            let kind = HookKind::from_name(name);
            assert_eq!(kind.as_name(), name);
        }
        let unknown = HookKind::from_name("FutureHook");
        assert_eq!(unknown.as_name(), "FutureHook");
        assert!(matches!(unknown, HookKind::Unknown(_)));
    }

    #[test]
    fn hook_response_helpers_round_trip() {
        let cont = HookResponse::r#continue();
        assert!(cont.r#continue);
        assert!(cont.decision.is_none());

        let blocked = HookResponse::block("because");
        assert!(!blocked.r#continue);
        match blocked.decision.as_ref().unwrap() {
            HookDecision::Block { reason } => assert_eq!(reason, "because"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn permission_result_convenience_constructors() {
        let a = PermissionResult::allow();
        assert!(matches!(
            a,
            PermissionResult::Allow {
                scope: PermissionScope::Once,
                ..
            }
        ));
        let a2 = PermissionResult::allow_with_scope(PermissionScope::Always);
        assert!(matches!(
            a2,
            PermissionResult::Allow {
                scope: PermissionScope::Always,
                ..
            }
        ));
        let d = PermissionResult::deny("no");
        match d {
            PermissionResult::Deny { message, interrupt } => {
                assert_eq!(message, "no");
                assert!(!interrupt);
            },
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            PermissionResult::cancel(),
            PermissionResult::Cancel
        ));
    }

    #[test]
    fn tool_permission_context_bare_has_sensible_defaults() {
        let ctx = ToolPermissionContext::bare();
        assert!(ctx.suggestions_json.is_none());
        assert!(ctx.tool_use_id.is_none());
        assert!(ctx.tool_title.is_empty());
        assert!(matches!(ctx.tool_kind, ToolKind::Other));
        assert!(ctx.tool_content.is_empty());
        assert!(ctx.tool_locations.is_empty());
    }
}
