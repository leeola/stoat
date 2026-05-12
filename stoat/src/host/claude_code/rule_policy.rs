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

use super::{
    permission::{PermissionCallback, PermissionResult, ToolPermissionContext},
    permission_prompt::{ApprovalDecision, PermissionPrompt},
};
use async_trait::async_trait;
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};
use stoat_config::ToolPermissions;
use tokio::sync::{mpsc, oneshot};

/// Fallback Deny message used when an `always_confirm` rule matches
/// but no prompt channel is wired (tests, headless runs).
const CONFIRM_DENY_MESSAGE: &str =
    "this command requires confirmation; no approval channel is wired";

/// Compiled permission rules for a single tool.
struct CompiledRules {
    always_allow: Vec<Regex>,
    always_confirm: Vec<Regex>,
    always_deny: Vec<Regex>,
}

/// Process-lifetime mutable state populated by user choices in the
/// approval modal. Lives behind a [`Mutex`] because the policy is
/// shared across tasks via [`Arc`].
#[derive(Default)]
struct RuntimeState {
    /// Exact `(tool, primary_input)` pairs the user explicitly
    /// approved with the `Allow` button.
    session_allowed: HashSet<(String, String)>,
    /// Per-tool runtime always_allow regex patterns added by the
    /// `Always-allow` button.
    runtime_allow: HashMap<String, Vec<Regex>>,
}

/// Per-tool rule-based [`PermissionCallback`].
pub struct RuleBasedPolicy {
    rules: BTreeMap<String, CompiledRules>,
    runtime: Arc<Mutex<RuntimeState>>,
    prompt_tx: Option<mpsc::Sender<PermissionPrompt>>,
}

impl RuleBasedPolicy {
    /// Compiles `permissions` into a runnable policy. Patterns that
    /// fail to compile are logged via `tracing::warn!` and dropped;
    /// other rules in the same tool entry remain active. Confirm
    /// matches return Deny because no UI channel is wired.
    pub fn from_settings(permissions: &BTreeMap<String, ToolPermissions>) -> Self {
        Self {
            rules: compile_rules(permissions),
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
            prompt_tx: None,
        }
    }

    /// Like [`Self::from_settings`] but routes `always_confirm`
    /// matches through `prompt_tx`. The receiver lives on the UI
    /// thread; user choices flow back via the prompt's
    /// `oneshot::Sender`.
    pub fn with_prompt_channel(
        permissions: &BTreeMap<String, ToolPermissions>,
        prompt_tx: mpsc::Sender<PermissionPrompt>,
    ) -> Self {
        Self {
            rules: compile_rules(permissions),
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
            prompt_tx: Some(prompt_tx),
        }
    }

    async fn prompt_or_deny(
        &self,
        tool_name: &str,
        target: &str,
        input_json: &str,
    ) -> PermissionResult {
        let Some(prompt_tx) = &self.prompt_tx else {
            return PermissionResult::Deny {
                message: CONFIRM_DENY_MESSAGE.to_string(),
                interrupt: false,
            };
        };

        let (response_tx, response_rx) = oneshot::channel();
        let prompt = PermissionPrompt {
            tool: tool_name.to_string(),
            input: input_json.to_string(),
            response_tx,
        };
        if prompt_tx.send(prompt).await.is_err() {
            tracing::warn!(
                target: "stoat::permission",
                %tool_name,
                "approval channel closed; denying confirm match",
            );
            return PermissionResult::Deny {
                message: "approval channel closed".to_string(),
                interrupt: false,
            };
        }

        let decision = match response_rx.await {
            Ok(d) => d,
            Err(_) => {
                tracing::warn!(
                    target: "stoat::permission",
                    %tool_name,
                    "approval response dropped; denying confirm match",
                );
                return PermissionResult::Deny {
                    message: "approval cancelled".to_string(),
                    interrupt: false,
                };
            },
        };

        match decision {
            ApprovalDecision::AllowOnce => PermissionResult::allow(),
            ApprovalDecision::Allow => {
                let mut runtime = self.runtime.lock().expect("runtime mutex poisoned");
                runtime
                    .session_allowed
                    .insert((tool_name.to_string(), target.to_string()));
                PermissionResult::allow()
            },
            ApprovalDecision::AlwaysAllow => {
                let literal = regex::escape(target);
                let pattern = format!("^{literal}$");
                match Regex::new(&pattern) {
                    Ok(re) => {
                        let mut runtime = self.runtime.lock().expect("runtime mutex poisoned");
                        runtime
                            .runtime_allow
                            .entry(tool_name.to_string())
                            .or_default()
                            .push(re);
                    },
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::permission",
                            %tool_name,
                            %err,
                            "failed to compile literal-match regex for always-allow; falling back to allow-once",
                        );
                    },
                }
                PermissionResult::allow()
            },
            ApprovalDecision::Deny => PermissionResult::Deny {
                message: format!("user denied {tool_name} via approval modal"),
                interrupt: false,
            },
        }
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
        let target = extract_primary_input(tool_name, input_json);

        if let Some(compiled) = self.rules.get(tool_name) {
            if any_match(&compiled.always_deny, &target) {
                return PermissionResult::Deny {
                    message: format!("denied by always_deny rule on {tool_name}"),
                    interrupt: false,
                };
            }
        }

        {
            let runtime = self.runtime.lock().expect("runtime mutex poisoned");
            if runtime
                .session_allowed
                .contains(&(tool_name.to_string(), target.clone()))
            {
                return PermissionResult::allow();
            }
            if let Some(patterns) = runtime.runtime_allow.get(tool_name) {
                if any_match(patterns, &target) {
                    return PermissionResult::allow();
                }
            }
        }

        let Some(compiled) = self.rules.get(tool_name) else {
            return PermissionResult::allow();
        };
        if any_match(&compiled.always_confirm, &target) {
            return self.prompt_or_deny(tool_name, &target, input_json).await;
        }
        if any_match(&compiled.always_allow, &target) {
            return PermissionResult::allow();
        }
        PermissionResult::allow()
    }
}

fn compile_rules(
    permissions: &BTreeMap<String, ToolPermissions>,
) -> BTreeMap<String, CompiledRules> {
    let mut rules = BTreeMap::new();
    for (tool, raw) in permissions {
        let compiled = CompiledRules {
            always_allow: compile_patterns(tool, "always_allow", &raw.always_allow),
            always_confirm: compile_patterns(tool, "always_confirm", &raw.always_confirm),
            always_deny: compile_patterns(tool, "always_deny", &raw.always_deny),
        };
        rules.insert(tool.clone(), compiled);
    }
    rules
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

    fn rules_with_prompt(
        entries: &[(&str, ToolPermissions)],
    ) -> (RuleBasedPolicy, mpsc::Receiver<PermissionPrompt>) {
        let map: BTreeMap<String, ToolPermissions> = entries
            .iter()
            .map(|(name, perms)| ((*name).to_string(), perms.clone()))
            .collect();
        let (tx, rx) = mpsc::channel(8);
        (RuleBasedPolicy::with_prompt_channel(&map, tx), rx)
    }

    /// Spawns a future driving `policy.can_use_tool(...)` and returns
    /// the prompt that comes out the rx side. The test side then
    /// answers with `decision` and awaits the future to get the
    /// `PermissionResult`.
    async fn drive_prompt(
        policy: Arc<RuleBasedPolicy>,
        rx: &mut mpsc::Receiver<PermissionPrompt>,
        tool: &'static str,
        input_json: &'static str,
        decision: ApprovalDecision,
    ) -> PermissionResult {
        let task_policy = policy.clone();
        let join = tokio::spawn(async move {
            task_policy
                .can_use_tool(tool, input_json, ToolPermissionContext::bare())
                .await
        });
        let prompt = rx.recv().await.expect("prompt should arrive");
        prompt.response_tx.send(decision).expect("send decision");
        join.await.expect("policy task")
    }

    #[tokio::test]
    async fn confirm_without_channel_returns_deny_with_message() {
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
    async fn confirm_allow_once_returns_allow_without_state_change() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);
        let first = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::AllowOnce,
        )
        .await;
        assert!(matches!(first, PermissionResult::Allow { .. }));

        let second = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::AllowOnce,
        )
        .await;
        assert!(matches!(second, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn confirm_allow_caches_exact_input() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);

        let first = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::Allow,
        )
        .await;
        assert!(matches!(first, PermissionResult::Allow { .. }));

        let second = policy
            .can_use_tool(
                "Bash",
                r#"{"command": "cargo install ripgrep"}"#,
                ToolPermissionContext::bare(),
            )
            .await;
        assert!(matches!(second, PermissionResult::Allow { .. }));
        assert!(rx.try_recv().is_err(), "second call should not prompt");
    }

    #[tokio::test]
    async fn confirm_allow_only_caches_exact_input_not_other_inputs() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);

        let first = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::Allow,
        )
        .await;
        assert!(matches!(first, PermissionResult::Allow { .. }));

        let second = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install fd-find"}"#,
            ApprovalDecision::AllowOnce,
        )
        .await;
        assert!(matches!(second, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn confirm_always_allow_caches_literal_pattern() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);

        let first = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::AlwaysAllow,
        )
        .await;
        assert!(matches!(first, PermissionResult::Allow { .. }));

        let second = policy
            .can_use_tool(
                "Bash",
                r#"{"command": "cargo install ripgrep"}"#,
                ToolPermissionContext::bare(),
            )
            .await;
        assert!(matches!(second, PermissionResult::Allow { .. }));
        assert!(rx.try_recv().is_err(), "second call should not prompt");
    }

    #[tokio::test]
    async fn confirm_deny_returns_deny_without_caching() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);

        let first = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::Deny,
        )
        .await;
        assert!(matches!(first, PermissionResult::Deny { .. }));

        let second = drive_prompt(
            policy.clone(),
            &mut rx,
            "Bash",
            r#"{"command": "cargo install ripgrep"}"#,
            ApprovalDecision::AllowOnce,
        )
        .await;
        assert!(matches!(second, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn dropped_response_sender_returns_deny() {
        let (policy, mut rx) =
            rules_with_prompt(&[("Bash", perms(&[], &["^cargo install "], &[]))]);
        let policy = Arc::new(policy);
        let task_policy = policy.clone();
        let join = tokio::spawn(async move {
            task_policy
                .can_use_tool(
                    "Bash",
                    r#"{"command": "cargo install ripgrep"}"#,
                    ToolPermissionContext::bare(),
                )
                .await
        });
        let prompt = rx.recv().await.expect("prompt arrives");
        drop(prompt.response_tx);
        let result = join.await.expect("policy task");
        assert!(matches!(result, PermissionResult::Deny { .. }));
    }
}
