//! Authentication helpers.
//!
//! Identifies remote SSH/headless environments and composes the
//! `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` /
//! `ANTHROPIC_CUSTOM_HEADERS` env vars required to route a CC session
//! through a custom gateway. Also catalogues the auth flows a host can
//! offer the user (Claude.ai subscription, Anthropic Console, CLI
//! login, custom gateway).

use std::{collections::HashMap, path::PathBuf};

/// An auth method the client can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// Claude subscription login (Claude Pro/Max).
    ClaudeAiLogin {
        args: Vec<String>,
        meta: Option<TerminalAuthMeta>,
    },
    /// Anthropic Console login (API billing).
    ConsoleLogin {
        args: Vec<String>,
        meta: Option<TerminalAuthMeta>,
    },
    /// Remote environment ssh/container login.
    ClaudeLogin {
        args: Vec<String>,
        meta: Option<TerminalAuthMeta>,
    },
    /// Custom OpenAI-compatible gateway.
    Gateway { protocol: String },
}

impl AuthMethod {
    pub fn id(&self) -> &'static str {
        match self {
            AuthMethod::ClaudeAiLogin { .. } => "claude-ai-login",
            AuthMethod::ConsoleLogin { .. } => "console-login",
            AuthMethod::ClaudeLogin { .. } => "claude-login",
            AuthMethod::Gateway { .. } => "gateway",
        }
    }
}

/// Structured terminal command the client can execute to drive the
/// auth flow. When `None`, the client should show an informational
/// prompt telling the user to run `claude /login` themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAuthMeta {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub label: String,
}

/// Custom-gateway credentials. `baseUrl` replaces the Anthropic API
/// endpoint; `headers` are forwarded on every request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GatewayAuth {
    pub base_url: String,
    pub headers: HashMap<String, String>,
}

/// Build the environment-variable overrides the CLI needs when routed
/// through a custom Anthropic-compatible gateway. Sets
/// `ANTHROPIC_BASE_URL`, joins headers into `ANTHROPIC_CUSTOM_HEADERS`,
/// and zeroes `ANTHROPIC_AUTH_TOKEN` so the subscription auth check is
/// bypassed.
pub fn create_env_for_gateway(gateway: Option<&GatewayAuth>) -> HashMap<String, String> {
    let Some(gw) = gateway else {
        return HashMap::new();
    };
    let mut env = HashMap::new();
    env.insert("ANTHROPIC_BASE_URL".to_string(), gw.base_url.clone());
    let headers_joined = gw
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    env.insert("ANTHROPIC_CUSTOM_HEADERS".to_string(), headers_joined);
    // Empty token bypasses the claude.ai subscription auth check.
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), String::new());
    env
}

/// Heuristically detect whether the host is running in a remote /
/// headless environment (SSH session, explicit `NO_BROWSER`,
/// container). A host uses this to prefer terminal-driven auth over
/// browser-based flows.
pub fn is_remote_environment() -> bool {
    std::env::var_os("NO_BROWSER").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("SSH_TTY").is_some()
}

/// List the auth methods appropriate for the current environment.
///
/// `support_gateway` toggles the `Gateway` option (enabled when the
/// host has a custom Anthropic-compatible endpoint configured);
/// `support_terminal_auth` toggles the terminal-driven login flows
/// (enabled when the host can spawn a child terminal for the user).
pub fn available_auth_methods(
    support_gateway: bool,
    support_terminal_auth: bool,
) -> Vec<AuthMethod> {
    let mut methods = Vec::new();
    if support_gateway {
        methods.push(AuthMethod::Gateway {
            protocol: "anthropic".into(),
        });
    }
    if is_remote_environment() {
        methods.push(AuthMethod::ClaudeLogin {
            args: vec!["--cli".into()],
            meta: support_terminal_auth.then(|| TerminalAuthMeta {
                command: PathBuf::from("claude"),
                args: vec!["--cli".into()],
                label: "Claude Login".into(),
            }),
        });
    } else {
        methods.push(AuthMethod::ClaudeAiLogin {
            args: vec![
                "--cli".into(),
                "auth".into(),
                "login".into(),
                "--claudeai".into(),
            ],
            meta: support_terminal_auth.then(|| TerminalAuthMeta {
                command: PathBuf::from("claude"),
                args: vec![
                    "--cli".into(),
                    "auth".into(),
                    "login".into(),
                    "--claudeai".into(),
                ],
                label: "Claude Login".into(),
            }),
        });
        methods.push(AuthMethod::ConsoleLogin {
            args: vec![
                "--cli".into(),
                "auth".into(),
                "login".into(),
                "--console".into(),
            ],
            meta: support_terminal_auth.then(|| TerminalAuthMeta {
                command: PathBuf::from("claude"),
                args: vec![
                    "--cli".into(),
                    "auth".into(),
                    "login".into(),
                    "--console".into(),
                ],
                label: "Anthropic Console Login".into(),
            }),
        });
    }
    methods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_env_builds_expected_keys() {
        let gw = GatewayAuth {
            base_url: "https://example.com".into(),
            headers: HashMap::from([
                ("X-Key".to_string(), "abc".to_string()),
                ("X-Other".to_string(), "def".to_string()),
            ]),
        };
        let env = create_env_for_gateway(Some(&gw));
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").unwrap(),
            "https://example.com"
        );
        assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN").unwrap(), "");
        let headers = env.get("ANTHROPIC_CUSTOM_HEADERS").unwrap();
        assert!(headers.contains("X-Key: abc"));
        assert!(headers.contains("X-Other: def"));
    }

    #[test]
    fn gateway_env_empty_when_no_gateway() {
        let env = create_env_for_gateway(None);
        assert!(env.is_empty());
    }

    #[test]
    fn available_methods_without_gateway() {
        // unsafe: modifying env for test scope only
        unsafe {
            std::env::remove_var("SSH_CONNECTION");
            std::env::remove_var("SSH_CLIENT");
            std::env::remove_var("SSH_TTY");
            std::env::remove_var("NO_BROWSER");
        }
        let methods = available_auth_methods(false, false);
        let ids: Vec<&str> = methods.iter().map(|m| m.id()).collect();
        assert!(ids.contains(&"claude-ai-login"));
        assert!(ids.contains(&"console-login"));
        assert!(!ids.contains(&"gateway"));
    }
}
