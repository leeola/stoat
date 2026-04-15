//! Slash-command filtering and transformation.
//!
//! Filters the list of slash commands the CLI advertises so Stoat
//! surfaces only ones it can meaningfully run, and routes local-only
//! commands (`/context`, `/heapdump`, `/extra-usage`) through the host
//! instead of letting the CLI consume a turn.

/// Commands the CLI advertises but Stoat drops (auth / release-notes /
/// keybinding helpers that don't make sense to expose through an
/// in-editor chat surface).
pub const UNSUPPORTED_COMMANDS: &[&str] = &[
    "cost",
    "keybindings-help",
    "login",
    "logout",
    "output-style:new",
    "release-notes",
    "todos",
];

/// Commands the CLI handles entirely locally without invoking the
/// model. Users that enter one of these should not be charged a turn
/// against their conversation budget.
pub const LOCAL_ONLY_COMMANDS: &[&str] = &["/context", "/heapdump", "/extra-usage"];

/// A single slash command surfaced to the host (for e.g. a command
/// palette).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// Raw command as the CLI reports it. The CLI uses `"Server (MCP)"`
/// for MCP-provided commands; this crate normalises to a
/// `"mcp:server:command"` shape so hosts don't have to parse the
/// suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<Vec<String>>,
}

/// Filter out unsupported commands and rewrite MCP names.
pub fn filter_and_transform_commands(cmds: Vec<CliSlashCommand>) -> Vec<AvailableCommand> {
    cmds.into_iter()
        .filter(|c| !UNSUPPORTED_COMMANDS.contains(&c.name.as_str()))
        .map(|c| {
            let name = normalize_mcp_command_name(&c.name);
            let input_hint = c.argument_hint.and_then(|hints| {
                if hints.is_empty() {
                    None
                } else {
                    Some(hints.join(" "))
                }
            });
            AvailableCommand {
                name,
                description: c.description,
                input_hint,
            }
        })
        .collect()
}

/// Return true if `text` (with or without leading `/`) matches a
/// local-only command.
pub fn is_local_only(text: &str) -> bool {
    let trimmed = text.trim();
    LOCAL_ONLY_COMMANDS
        .iter()
        .any(|cmd| trimmed == *cmd || trimmed.starts_with(&format!("{cmd} ")))
}

/// Rewrite `"Server (MCP)"` style names into `"mcp:server"`.
pub fn normalize_mcp_command_name(name: &str) -> String {
    let trimmed = name.trim();
    if let Some(stripped) = trimmed.strip_suffix(" (MCP)") {
        format!("mcp:{}", stripped.trim().to_lowercase().replace(' ', "_"))
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_removes_unsupported() {
        let input = vec![
            CliSlashCommand {
                name: "cost".into(),
                description: "".into(),
                argument_hint: None,
            },
            CliSlashCommand {
                name: "compact".into(),
                description: "Compact the context".into(),
                argument_hint: None,
            },
        ];
        let out = filter_and_transform_commands(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "compact");
    }

    #[test]
    fn filter_transforms_mcp_names() {
        let input = vec![CliSlashCommand {
            name: "Weather (MCP)".into(),
            description: "MCP weather tool".into(),
            argument_hint: Some(vec!["location".into()]),
        }];
        let out = filter_and_transform_commands(input);
        assert_eq!(out[0].name, "mcp:weather");
        assert_eq!(out[0].input_hint.as_deref(), Some("location"));
    }

    #[test]
    fn local_only_recognises_prefixes() {
        assert!(is_local_only("/context"));
        assert!(is_local_only("/context foo"));
        assert!(!is_local_only("/compact"));
    }
}
