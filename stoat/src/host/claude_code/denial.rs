//! Hardcoded denial policy for unconditionally dangerous Bash
//! invocations.
//!
//! Installed at the launcher boundary so every spawned Claude Code
//! session passes through this gate before any other permission rule
//! applies. The patterns are baked in source and have no runtime API
//! to add or remove. User-configurable allow/deny rules layer above
//! this gate; nothing can re-enable a denial issued here.

use super::permission::{PermissionCallback, PermissionResult, ToolPermissionContext};
use async_trait::async_trait;

/// [`PermissionCallback`] that refuses unconditionally-dangerous Bash
/// commands and allows every other tool call.
///
/// Returns [`PermissionResult::Deny`] with `interrupt: true` on a
/// match, so the agent run aborts entirely rather than letting Claude
/// retry a different phrasing.
pub struct BashDenialPolicy;

impl BashDenialPolicy {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BashDenialPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionCallback for BashDenialPolicy {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input_json: &str,
        _context: ToolPermissionContext<'_>,
    ) -> PermissionResult {
        if tool_name != "Bash" {
            return PermissionResult::allow();
        }
        let value: serde_json::Value = match serde_json::from_str(input_json) {
            Ok(v) => v,
            Err(_) => return PermissionResult::allow(),
        };
        let Some(command) = value.get("command").and_then(|v| v.as_str()) else {
            return PermissionResult::allow();
        };
        match denial_reason(command) {
            Some(reason) => PermissionResult::Deny {
                message: reason.to_string(),
                interrupt: true,
            },
            None => PermissionResult::allow(),
        }
    }
}

/// Token-based pattern match against the hardcoded denial list.
/// Returns a refusal message if the command matches one of:
///
/// - `rm -rf /` (any short-flag bundle including both `r`/`R` and `f`, with a target token equal to
///   `/`).
/// - `rm -rf ~` and `rm -rf $HOME`.
/// - `git push --force` / `-f` / `--force-with-lease` to a token equal to `main` or `master`, or a
///   refspec ending in `:main` / `:master`.
/// - Force-push refspecs prefixed with `+` (`+main`, `+HEAD:master`).
///
/// Shell wrappers (`bash -c "..."`, eval, pipes) are out of scope.
pub(crate) fn denial_reason(command: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] == "rm" {
        return rm_rf_root_or_home(&tokens[1..]);
    }
    if tokens[0] == "git" && tokens.get(1).copied() == Some("push") {
        return git_push_force_protected(&tokens[2..]);
    }
    None
}

fn rm_rf_root_or_home(args: &[&str]) -> Option<&'static str> {
    let mut recursive = false;
    let mut force = false;
    let mut targets: Vec<&str> = Vec::new();

    for arg in args {
        if let Some(short_flags) = short_flag_letters(arg) {
            if short_flags.contains('r') || short_flags.contains('R') {
                recursive = true;
            }
            if short_flags.contains('f') {
                force = true;
            }
        } else if *arg == "--recursive" {
            recursive = true;
        } else if *arg == "--force" {
            force = true;
        } else if !arg.starts_with("--") {
            targets.push(arg);
        }
    }

    if !(recursive && force) {
        return None;
    }
    for t in &targets {
        match *t {
            "/" => return Some("rm -rf / is denied: deletes the root filesystem"),
            "~" | "$HOME" => {
                return Some("rm -rf $HOME is denied: deletes the user's home directory");
            },
            _ => {},
        }
    }
    None
}

/// Short-flag bundle letters (`-rf` -> `Some("rf")`). Returns `None`
/// for long flags (`--force`) and non-flag arguments.
fn short_flag_letters(arg: &str) -> Option<&str> {
    let rest = arg.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    Some(rest)
}

fn git_push_force_protected(args: &[&str]) -> Option<&'static str> {
    let force_flag = args.iter().any(|a| {
        *a == "--force"
            || a.starts_with("--force-with-lease")
            || short_flag_letters(a).is_some_and(|f| f.contains('f'))
    });

    let plain_protected = args
        .iter()
        .any(|a| matches!(*a, "main" | "master") || a.ends_with(":main") || a.ends_with(":master"));

    let force_refspec_protected = args.iter().any(|a| {
        let Some(rest) = a.strip_prefix('+') else {
            return false;
        };
        matches!(rest, "main" | "master") || rest.ends_with(":main") || rest.ends_with(":master")
    });

    if force_refspec_protected || (force_flag && plain_protected) {
        Some("git push --force to main/master is denied: protected branch")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_rf_root_denies() {
        assert!(denial_reason("rm -rf /").is_some());
        assert!(denial_reason("rm -fr /").is_some());
        assert!(denial_reason("rm -Rf /").is_some());
        assert!(denial_reason("rm --recursive --force /").is_some());
        assert!(denial_reason("rm -rf --no-preserve-root /").is_some());
    }

    #[test]
    fn rm_rf_subpath_allows() {
        assert!(denial_reason("rm -rf /tmp").is_none());
        assert!(denial_reason("rm -rf /home/user").is_none());
        assert!(denial_reason("rm -rf ./build").is_none());
    }

    #[test]
    fn rm_rf_home_denies() {
        assert!(denial_reason("rm -rf ~").is_some());
        assert!(denial_reason("rm -rf $HOME").is_some());
    }

    #[test]
    fn rm_without_both_flags_allows() {
        assert!(denial_reason("rm -r /").is_none());
        assert!(denial_reason("rm -f /").is_none());
        assert!(denial_reason("rm /").is_none());
    }

    #[test]
    fn git_push_force_to_main_denies() {
        assert!(denial_reason("git push --force origin main").is_some());
        assert!(denial_reason("git push -f origin main").is_some());
        assert!(denial_reason("git push -uf origin main").is_some());
        assert!(denial_reason("git push --force-with-lease origin main").is_some());
        assert!(denial_reason("git push --force-with-lease=ref origin master").is_some());
        assert!(denial_reason("git push --force origin HEAD:main").is_some());
    }

    #[test]
    fn git_push_force_refspec_denies() {
        assert!(denial_reason("git push origin +main").is_some());
        assert!(denial_reason("git push origin +HEAD:master").is_some());
    }

    #[test]
    fn git_push_to_main_without_force_allows() {
        assert!(denial_reason("git push origin main").is_none());
        assert!(denial_reason("git push origin HEAD:main").is_none());
    }

    #[test]
    fn git_push_force_to_feature_allows() {
        assert!(denial_reason("git push --force origin feature").is_none());
        assert!(denial_reason("git push -f origin feature/x").is_none());
    }

    #[test]
    fn empty_command_allows() {
        assert!(denial_reason("").is_none());
        assert!(denial_reason("   ").is_none());
    }

    #[tokio::test]
    async fn callback_denies_dangerous_bash() {
        let policy = BashDenialPolicy::new();
        let result = policy
            .can_use_tool(
                "Bash",
                r#"{"command": "rm -rf /"}"#,
                ToolPermissionContext::bare(),
            )
            .await;
        match result {
            PermissionResult::Deny { interrupt, .. } => assert!(interrupt),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn callback_allows_safe_bash() {
        let policy = BashDenialPolicy::new();
        let result = policy
            .can_use_tool(
                "Bash",
                r#"{"command": "ls -la"}"#,
                ToolPermissionContext::bare(),
            )
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn callback_allows_non_bash_tool() {
        let policy = BashDenialPolicy::new();
        let result = policy
            .can_use_tool(
                "Read",
                r#"{"file_path": "/etc/passwd"}"#,
                ToolPermissionContext::bare(),
            )
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn callback_allows_malformed_input() {
        let policy = BashDenialPolicy::new();
        let result = policy
            .can_use_tool("Bash", "not json", ToolPermissionContext::bare())
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }
}
