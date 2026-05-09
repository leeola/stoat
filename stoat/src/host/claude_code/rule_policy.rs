//! User-configurable per-tool permission rules.
//!
//! [`RuleBasedPolicy`] holds compiled regex lists keyed by tool name
//! and consults them on every `can_use_tool` callback. Rules come
//! from stcfg via `claude.permissions.<tool>.always_allow` /
//! `always_confirm` / `always_deny` array settings; the launcher
//! installs this policy alongside [`super::denial::BashDenialPolicy`]
//! through a [`super::chain::ChainedPermissionPolicy`].
//!
//! Matching follows the precedence Zed documents in
//! `references/zed/docs/src/ai/tool-permissions.md`: deny first,
//! then confirm, then allow. A no-match falls through to
//! [`PermissionResult::allow`], which lets the chain continue or
//! the default-Allow take effect.
//!
//! The `always_confirm` outcome currently maps to a non-interrupt
//! [`PermissionResult::Deny`] with a message explaining that the
//! approval modal is not yet implemented (see TODO item 50). Once
//! the modal lands, that arm switches to routing the call into the
//! modal -- no schema change required.

use super::permission::{PermissionCallback, PermissionResult, ToolPermissionContext};
use async_trait::async_trait;
use regex::Regex;
use std::collections::BTreeMap;
use stoat_config::ToolPermissions;

const CONFIRM_DENY_MESSAGE: &str =
    "this command requires confirmation; approval modal not yet implemented";

/// Compiled permission rules for a single tool.
struct CompiledRules {
    always_allow: Vec<Regex>,
    always_confirm: Vec<Regex>,
    always_deny: Vec<Regex>,
}

/// Per-tool rule-based [`PermissionCallback`].
pub struct RuleBasedPolicy {
    rules: BTreeMap<String, CompiledRules>,
}

impl RuleBasedPolicy {
    /// Compiles `permissions` into a runnable policy. Patterns that
    /// fail to compile are logged via `tracing::warn!` and dropped;
    /// other rules in the same tool entry remain active.
    pub fn from_settings(permissions: &BTreeMap<String, ToolPermissions>) -> Self {
        let mut rules = BTreeMap::new();
        for (tool, raw) in permissions {
            let compiled = CompiledRules {
                always_allow: compile_patterns(tool, "always_allow", &raw.always_allow),
                always_confirm: compile_patterns(tool, "always_confirm", &raw.always_confirm),
                always_deny: compile_patterns(tool, "always_deny", &raw.always_deny),
            };
            rules.insert(tool.clone(), compiled);
        }
        Self { rules }
    }
}

#[async_trait]
impl PermissionCallback for RuleBasedPolicy {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input_json: &str,
        _context: ToolPermissionContext<'_>,
    ) -> PermissionResult {
        let Some(compiled) = self.rules.get(tool_name) else {
            return PermissionResult::allow();
        };
        let target = extract_primary_input(tool_name, input_json);
        if any_match(&compiled.always_deny, &target) {
            return PermissionResult::Deny {
                message: format!("denied by always_deny rule on {tool_name}"),
                interrupt: false,
            };
        }
        if any_match(&compiled.always_confirm, &target) {
            return PermissionResult::Deny {
                message: CONFIRM_DENY_MESSAGE.to_string(),
                interrupt: false,
            };
        }
        if any_match(&compiled.always_allow, &target) {
            return PermissionResult::allow();
        }
        PermissionResult::allow()
    }
}

fn compile_patterns(tool: &str, behavior: &str, patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pattern| match Regex::new(pattern) {
            Ok(re) => Some(re),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::permission",
                    %tool,
                    %behavior,
                    %pattern,
                    %err,
                    "claude permission pattern failed to compile; ignoring",
                );
                None
            },
        })
        .collect()
}

fn any_match(patterns: &[Regex], target: &str) -> bool {
    patterns.iter().any(|re| re.is_match(target))
}

/// Returns the string a permission rule matches against for a given
/// tool. Falls back to the raw JSON input so unrecognised tools are
/// still matchable (useful for MCP tools and future additions).
fn extract_primary_input(tool_name: &str, input_json: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return input_json.to_string(),
    };
    let field = match tool_name {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_path",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Glob" | "Grep" => "pattern",
        _ => return input_json.to_string(),
    };
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| input_json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_config::ToolPermissions;

    fn rules(entries: &[(&str, ToolPermissions)]) -> RuleBasedPolicy {
        let map: BTreeMap<String, ToolPermissions> = entries
            .iter()
            .map(|(name, perms)| ((*name).to_string(), perms.clone()))
            .collect();
        RuleBasedPolicy::from_settings(&map)
    }

    fn perms(allow: &[&str], confirm: &[&str], deny: &[&str]) -> ToolPermissions {
        ToolPermissions {
            always_allow: allow.iter().map(|s| (*s).to_string()).collect(),
            always_confirm: confirm.iter().map(|s| (*s).to_string()).collect(),
            always_deny: deny.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    async fn run(policy: &RuleBasedPolicy, tool: &str, input_json: &str) -> PermissionResult {
        policy
            .can_use_tool(tool, input_json, ToolPermissionContext::bare())
            .await
    }

    #[tokio::test]
    async fn deny_blocks_matching_bash_command() {
        let policy = rules(&[("Bash", perms(&[], &[], &["^sudo "]))]);
        let result = run(&policy, "Bash", r#"{"command": "sudo apt update"}"#).await;
        match result {
            PermissionResult::Deny { interrupt, .. } => assert!(!interrupt),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_returns_modal_pending_deny() {
        let policy = rules(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let result = run(&policy, "Bash", r#"{"command": "cargo install ripgrep"}"#).await;
        match result {
            PermissionResult::Deny { message, interrupt } => {
                assert_eq!(message, CONFIRM_DENY_MESSAGE);
                assert!(!interrupt);
            },
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_returns_allow() {
        let policy = rules(&[("Bash", perms(&["^cargo (build|test)"], &[], &[]))]);
        let result = run(&policy, "Bash", r#"{"command": "cargo build"}"#).await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn deny_wins_over_confirm() {
        let policy = rules(&[("Bash", perms(&[], &["^cargo "], &["^cargo install "]))]);
        let result = run(&policy, "Bash", r#"{"command": "cargo install ripgrep"}"#).await;
        match result {
            PermissionResult::Deny { message, .. } => {
                assert!(message.starts_with("denied by always_deny"));
            },
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_wins_over_allow() {
        let policy = rules(&[("Bash", perms(&["^cargo "], &["^cargo install "], &[]))]);
        let result = run(&policy, "Bash", r#"{"command": "cargo install ripgrep"}"#).await;
        match result {
            PermissionResult::Deny { message, .. } => assert_eq!(message, CONFIRM_DENY_MESSAGE),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_rules_for_tool_allows() {
        let policy = rules(&[("Bash", perms(&[], &[], &["^sudo "]))]);
        let result = run(&policy, "Read", r#"{"file_path": "/etc/passwd"}"#).await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn read_matches_against_file_path() {
        let policy = rules(&[("Read", perms(&[], &[], &["secrets/"]))]);
        let result = run(&policy, "Read", r#"{"file_path": "/repo/secrets/api.key"}"#).await;
        match result {
            PermissionResult::Deny { .. } => {},
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_tool_falls_back_to_json_match() {
        let policy = rules(&[("CustomMcp", perms(&[], &[], &["dangerous"]))]);
        let result = run(
            &policy,
            "CustomMcp",
            r#"{"action": "do_something_dangerous"}"#,
        )
        .await;
        match result {
            PermissionResult::Deny { .. } => {},
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_falls_back_to_raw_string() {
        let policy = rules(&[("Bash", perms(&[], &[], &["needle"]))]);
        let result = run(&policy, "Bash", "this is needle in haystack").await;
        match result {
            PermissionResult::Deny { .. } => {},
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_regex_dropped_others_still_match() {
        let policy = rules(&[("Bash", perms(&[], &[], &["[invalid", "^sudo "]))]);
        let result = run(&policy, "Bash", r#"{"command": "sudo rm /tmp/foo"}"#).await;
        match result {
            PermissionResult::Deny { .. } => {},
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_anywhere_returns_allow() {
        let policy = rules(&[("Bash", perms(&["^cargo "], &["^npm "], &["^sudo "]))]);
        let result = run(&policy, "Bash", r#"{"command": "ls -la"}"#).await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }
}
