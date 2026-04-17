//! Tool classification and display metadata.
//!
//! The Claude CLI emits `tool_use` content blocks inside assistant
//! messages. The `name` string alone is not enough for a UI to render
//! a useful tool call: it needs to know what *kind* of tool it is
//! (read, edit, execute, ...), what title to show, what inline content
//! to synthesize (diff, terminal widget, prompt preview), and which
//! file locations to link to. This module does that classification,
//! plus synthesizes [`ToolUpdate`]s from `tool_result` payloads
//! (including Edit/Write `structuredPatch` -> [`ToolCallContent::Diff`]).
//!
//! The shared classification types ([`ToolKind`], [`ToolCallContent`],
//! [`PlanEntry`], [`TerminalMeta`], ...) live in `stoat::host` so both
//! the agent and the consumer crate can reference them without a
//! circular dependency. This module re-exports them for convenience.

use crate::messages::ToolUse;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
pub use stoat::host::{ModeInfo, ModelInfo, TokenUsage};
use stoat::host::{
    PlanEntry, PlanEntryStatus, TerminalMeta, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolKind,
};

/// Classifier output: the tool call decorated with title, kind, and
/// zero or more pieces of renderable content.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub title: String,
    pub kind: ToolKind,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
}

/// Incremental update produced when a tool's final state arrives (via
/// `tool_result` or a PostToolUse hook).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolUpdate {
    pub title: Option<String>,
    pub content: Option<Vec<ToolCallContent>>,
    pub locations: Option<Vec<ToolCallLocation>>,
    pub status: Option<ToolCallStatus>,
    pub terminal_meta: Option<TerminalMeta>,
}

/// Snapshot of a seen `tool_use` content block. Stored per-session so
/// later classification passes (streaming refinement, tool_result) can
/// look up the original name/input without walking the message history.
#[derive(Debug, Clone)]
pub struct ToolUseSnapshot {
    pub id: String,
    pub name: String,
    pub input: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------
// Classifier: tool_info_from_tool_use
// ---------------------------------------------------------------------

const MAX_TITLE_LEN: usize = 256;

/// Classify a tool_use block.
///
/// `supports_terminal` controls whether Bash tool calls produce a
/// [`ToolCallContent::Terminal`] (if true) or a plain text description
/// (if false). `cwd` shortens file-path titles when the path falls under
/// the session's working directory.
pub fn tool_info_from_tool_use(
    tool: &ToolUse,
    supports_terminal: bool,
    cwd: Option<&Path>,
) -> ToolInfo {
    let name = tool.name.as_str();
    let input = &tool.input;

    match name {
        "Agent" | "Task" => classify_task(name, input),
        "Bash" => classify_bash(&tool.id, input, supports_terminal),
        "Read" | "NotebookRead" => classify_read(name, input, cwd),
        "Write" => classify_write(input, cwd),
        "Edit" => classify_edit(input, cwd),
        "MultiEdit" => classify_multi_edit(input, cwd),
        "NotebookEdit" => classify_notebook_edit(input, cwd),
        "Glob" => classify_glob(input, cwd),
        "Grep" => classify_grep(input),
        "WebFetch" => classify_web_fetch(input),
        "WebSearch" => classify_web_search(input),
        "TodoWrite" => ToolInfo {
            title: "Update plan".into(),
            kind: ToolKind::Think,
            content: vec![],
            locations: vec![],
        },
        "ExitPlanMode" => classify_exit_plan_mode(input),
        "Skill" => classify_skill(input),
        _ if name.starts_with("mcp__") => classify_mcp_tool(name, input),
        _ => classify_fallback(name, input),
    }
}

fn classify_task(default_title: &str, input: &Input) -> ToolInfo {
    let prompt = string_field(input, "prompt").unwrap_or_default();
    let description =
        string_field(input, "description").unwrap_or_else(|| default_title.to_string());
    let content = if !prompt.is_empty() {
        vec![ToolCallContent::Text {
            text: truncate(&prompt, MAX_TITLE_LEN * 4),
        }]
    } else {
        vec![]
    };
    ToolInfo {
        title: truncate(&description, MAX_TITLE_LEN),
        kind: ToolKind::Think,
        content,
        locations: vec![],
    }
}

fn classify_bash(tool_id: &str, input: &Input, supports_terminal: bool) -> ToolInfo {
    let command = string_field(input, "command").unwrap_or_default();
    let description = string_field(input, "description").unwrap_or_default();

    let title = if !description.is_empty() {
        truncate(&description, MAX_TITLE_LEN)
    } else {
        truncate(&first_line(&command), MAX_TITLE_LEN)
    };

    let content = if supports_terminal {
        vec![ToolCallContent::Terminal {
            terminal_id: tool_id.to_string(),
        }]
    } else if !command.is_empty() {
        vec![ToolCallContent::Text {
            text: format!("```bash\n{command}\n```"),
        }]
    } else {
        vec![]
    };

    ToolInfo {
        title,
        kind: ToolKind::Execute,
        content,
        locations: vec![],
    }
}

fn classify_read(name: &str, input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let path = string_field(input, "file_path")
        .or_else(|| string_field(input, "notebook_path"))
        .unwrap_or_default();
    let offset = integer_field(input, "offset");
    let limit = integer_field(input, "limit");

    let display_path = to_display_path(Path::new(&path), cwd);
    let range_suffix = match (offset, limit) {
        (Some(o), Some(l)) => format!(" ({o}-{})", o.saturating_add(l)),
        (Some(o), None) => format!(" (from {o})"),
        _ => String::new(),
    };

    let title = if path.is_empty() {
        name.to_string()
    } else {
        truncate(
            &format!("{name} {display_path}{range_suffix}"),
            MAX_TITLE_LEN,
        )
    };

    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: PathBuf::from(&path),
            line: offset.map(|v| v as u32),
        }]
    };

    ToolInfo {
        title,
        kind: ToolKind::Read,
        content: vec![],
        locations,
    }
}

fn classify_write(input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let path = string_field(input, "file_path").unwrap_or_default();
    let content_text = string_field(input, "content").unwrap_or_default();
    let display_path = to_display_path(Path::new(&path), cwd);

    let title = if path.is_empty() {
        "Write".into()
    } else {
        truncate(&format!("Write {display_path}"), MAX_TITLE_LEN)
    };

    let content = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallContent::Diff {
            path: PathBuf::from(&path),
            old_text: None,
            new_text: content_text,
        }]
    };

    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: PathBuf::from(&path),
            line: None,
        }]
    };

    ToolInfo {
        title,
        kind: ToolKind::Edit,
        content,
        locations,
    }
}

fn classify_edit(input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let path = string_field(input, "file_path").unwrap_or_default();
    let old_text = string_field(input, "old_string").unwrap_or_default();
    let new_text = string_field(input, "new_string").unwrap_or_default();
    let display_path = to_display_path(Path::new(&path), cwd);

    let title = if path.is_empty() {
        "Edit".into()
    } else {
        truncate(&format!("Edit {display_path}"), MAX_TITLE_LEN)
    };

    let content = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallContent::Diff {
            path: PathBuf::from(&path),
            old_text: Some(old_text),
            new_text,
        }]
    };

    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: PathBuf::from(&path),
            line: None,
        }]
    };

    ToolInfo {
        title,
        kind: ToolKind::Edit,
        content,
        locations,
    }
}

fn classify_multi_edit(input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let path = string_field(input, "file_path").unwrap_or_default();
    let display_path = to_display_path(Path::new(&path), cwd);

    let edits = input.get("edits").and_then(|v| v.as_array());
    let content: Vec<ToolCallContent> = edits
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let old = entry.get("old_string")?.as_str()?.to_string();
                    let new = entry.get("new_string")?.as_str()?.to_string();
                    Some(ToolCallContent::Diff {
                        path: PathBuf::from(&path),
                        old_text: Some(old),
                        new_text: new,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let title = if path.is_empty() {
        "MultiEdit".into()
    } else {
        truncate(&format!("MultiEdit {display_path}"), MAX_TITLE_LEN)
    };

    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: PathBuf::from(&path),
            line: None,
        }]
    };

    ToolInfo {
        title,
        kind: ToolKind::Edit,
        content,
        locations,
    }
}

fn classify_notebook_edit(input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let path = string_field(input, "notebook_path").unwrap_or_default();
    let display_path = to_display_path(Path::new(&path), cwd);
    let title = if path.is_empty() {
        "NotebookEdit".into()
    } else {
        truncate(&format!("NotebookEdit {display_path}"), MAX_TITLE_LEN)
    };
    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: PathBuf::from(&path),
            line: None,
        }]
    };
    ToolInfo {
        title,
        kind: ToolKind::Edit,
        content: vec![],
        locations,
    }
}

fn classify_glob(input: &Input, cwd: Option<&Path>) -> ToolInfo {
    let pattern = string_field(input, "pattern").unwrap_or_default();
    let path = string_field(input, "path").unwrap_or_default();

    let title = if pattern.is_empty() {
        "Glob".into()
    } else {
        truncate(&format!("Find `{pattern}`"), MAX_TITLE_LEN)
    };

    let locations = if path.is_empty() {
        vec![]
    } else {
        vec![ToolCallLocation {
            path: cwd
                .map(|c| c.join(&path))
                .unwrap_or_else(|| PathBuf::from(&path)),
            line: None,
        }]
    };

    ToolInfo {
        title,
        kind: ToolKind::Search,
        content: vec![],
        locations,
    }
}

fn classify_grep(input: &Input) -> ToolInfo {
    let mut cmd = String::from("grep");
    if bool_field(input, "-i") == Some(true) {
        cmd.push_str(" -i");
    }
    if let Some(before) = integer_field(input, "-B") {
        cmd.push_str(&format!(" -B {before}"));
    }
    if let Some(after) = integer_field(input, "-A") {
        cmd.push_str(&format!(" -A {after}"));
    }
    if let Some(pattern) = string_field(input, "pattern") {
        cmd.push(' ');
        cmd.push_str(&pattern);
    }
    ToolInfo {
        title: truncate(&cmd, MAX_TITLE_LEN),
        kind: ToolKind::Search,
        content: vec![],
        locations: vec![],
    }
}

fn classify_web_fetch(input: &Input) -> ToolInfo {
    let url = string_field(input, "url").unwrap_or_default();
    let prompt = string_field(input, "prompt").unwrap_or_default();
    let title = if url.is_empty() {
        "Fetch".into()
    } else {
        truncate(&format!("Fetch {url}"), MAX_TITLE_LEN)
    };
    let content = if prompt.is_empty() {
        vec![]
    } else {
        vec![ToolCallContent::Text {
            text: truncate(&prompt, MAX_TITLE_LEN * 4),
        }]
    };
    ToolInfo {
        title,
        kind: ToolKind::Fetch,
        content,
        locations: vec![],
    }
}

fn classify_web_search(input: &Input) -> ToolInfo {
    let query = string_field(input, "query").unwrap_or_default();
    let allowed = input
        .get("allowed_domains")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let title = match (query.is_empty(), allowed.is_empty()) {
        (false, false) => truncate(&format!("{query} (allowed: {allowed})"), MAX_TITLE_LEN),
        (false, true) => truncate(&query, MAX_TITLE_LEN),
        _ => "WebSearch".into(),
    };
    ToolInfo {
        title,
        kind: ToolKind::Search,
        content: vec![],
        locations: vec![],
    }
}

fn classify_exit_plan_mode(input: &Input) -> ToolInfo {
    let plan = string_field(input, "plan").unwrap_or_default();
    let content = if plan.is_empty() {
        vec![]
    } else {
        vec![ToolCallContent::Text { text: plan }]
    };
    ToolInfo {
        title: "Ready to code?".into(),
        kind: ToolKind::SwitchMode,
        content,
        locations: vec![],
    }
}

fn classify_skill(input: &Input) -> ToolInfo {
    let name = string_field(input, "skill_name")
        .or_else(|| string_field(input, "name"))
        .unwrap_or_else(|| "Skill".into());
    let description = string_field(input, "description").unwrap_or_default();
    let content = if description.is_empty() {
        vec![]
    } else {
        vec![ToolCallContent::Text { text: description }]
    };
    ToolInfo {
        title: truncate(&name, MAX_TITLE_LEN),
        kind: ToolKind::Think,
        content,
        locations: vec![],
    }
}

fn classify_mcp_tool(name: &str, input: &Input) -> ToolInfo {
    // `mcp__server__tool_name`. We keep the full `name` as the title so
    // callers can display where it came from.
    let title = truncate(name, MAX_TITLE_LEN);
    let input_text = serde_json::to_string(input).unwrap_or_default();
    let content = if input_text.is_empty() || input_text == "{}" {
        vec![]
    } else {
        vec![ToolCallContent::Text {
            text: format!("```json\n{input_text}\n```"),
        }]
    };
    ToolInfo {
        title,
        kind: ToolKind::Other,
        content,
        locations: vec![],
    }
}

fn classify_fallback(name: &str, input: &Input) -> ToolInfo {
    let input_text = serde_json::to_string(input).unwrap_or_default();
    let content = if input_text.is_empty() || input_text == "{}" {
        vec![]
    } else {
        vec![ToolCallContent::Text {
            text: format!("```json\n{input_text}\n```"),
        }]
    };
    ToolInfo {
        title: truncate(name, MAX_TITLE_LEN),
        kind: ToolKind::Other,
        content,
        locations: vec![],
    }
}

// ---------------------------------------------------------------------
// Tool-result / PostToolUse-hook classification
// ---------------------------------------------------------------------

/// Build a [`ToolUpdate`] from a `tool_result` block. The original
/// `tool_use` is looked up in `cache` so the updater can format results
/// that depend on the tool's identity (e.g. Bash needs to know it was a
/// Bash invocation to emit terminal metadata).
pub fn tool_update_from_tool_result(
    tool_use_id: &str,
    result_text: &str,
    is_error: bool,
    cache: &HashMap<String, ToolUseSnapshot>,
    supports_terminal: bool,
) -> ToolUpdate {
    let status = if is_error {
        Some(ToolCallStatus::Failed)
    } else {
        Some(ToolCallStatus::Completed)
    };

    let Some(tool) = cache.get(tool_use_id) else {
        // Unknown tool: fall back to plain text content.
        return ToolUpdate {
            status,
            content: Some(vec![ToolCallContent::Text {
                text: result_text.to_string(),
            }]),
            ..Default::default()
        };
    };

    match tool.name.as_str() {
        "Bash" if supports_terminal => ToolUpdate {
            status,
            terminal_meta: Some(TerminalMeta {
                terminal_id: tool.id.clone(),
                output: Some(result_text.to_string()),
                exit_code: None,
                signal: None,
            }),
            ..Default::default()
        },
        "Bash" => ToolUpdate {
            status,
            content: Some(vec![ToolCallContent::Text {
                text: format!("```\n{result_text}\n```"),
            }]),
            ..Default::default()
        },
        _ => ToolUpdate {
            status,
            content: Some(vec![ToolCallContent::Text {
                text: result_text.to_string(),
            }]),
            ..Default::default()
        },
    }
}

/// Build a [`ToolUpdate`] from a PostToolUse hook payload for Edit-style
/// tools. Extracts the authoritative `structuredPatch` from the
/// tool_response and produces a [`ToolCallContent::Diff`].
pub fn tool_update_from_edit_tool_response(response: &serde_json::Value) -> ToolUpdate {
    let patch = response.get("structuredPatch").cloned();
    let file_path = response
        .get("filePath")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    // When structuredPatch is an array of hunks, flatten into one diff per
    // hunk. Fall back to old_string/new_string extraction when missing.
    let content = if let Some(serde_json::Value::Array(hunks)) = patch {
        hunks
            .into_iter()
            .map(|hunk| {
                let old_lines = hunk.get("oldLines").cloned();
                let new_lines = hunk.get("newLines").cloned();
                let old = lines_to_text(old_lines);
                let new = lines_to_text(new_lines);
                ToolCallContent::Diff {
                    path: PathBuf::from(&file_path),
                    old_text: Some(old),
                    new_text: new,
                }
            })
            .collect()
    } else if let (Some(old), Some(new)) = (
        response.get("oldString").and_then(|v| v.as_str()),
        response.get("newString").and_then(|v| v.as_str()),
    ) {
        vec![ToolCallContent::Diff {
            path: PathBuf::from(&file_path),
            old_text: Some(old.to_string()),
            new_text: new.to_string(),
        }]
    } else {
        vec![]
    };

    ToolUpdate {
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        status: Some(ToolCallStatus::Completed),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// Plan extraction (TodoWrite)
// ---------------------------------------------------------------------

/// Extract plan entries from a TodoWrite tool input.
pub fn plan_entries(input: &serde_json::Value) -> Vec<PlanEntry> {
    let Some(todos) = input.get("todos").and_then(|v| v.as_array()) else {
        return vec![];
    };
    todos
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content")?.as_str()?.to_string();
            let status_str = todo.get("status")?.as_str()?;
            let status = match status_str {
                "pending" => PlanEntryStatus::Pending,
                "in_progress" => PlanEntryStatus::InProgress,
                "completed" => PlanEntryStatus::Completed,
                _ => return None,
            };
            Some(PlanEntry {
                content,
                status,
                priority: "medium".into(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// Path / string helpers
// ---------------------------------------------------------------------

/// Shorten a path for display. If `cwd` is `Some` and `path` starts with
/// it, returns the relative portion. Otherwise returns the raw path.
pub fn to_display_path(path: &Path, cwd: Option<&Path>) -> String {
    if let Some(cwd) = cwd
        && let Ok(rel) = path.strip_prefix(cwd)
    {
        return rel.to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

/// Escape markdown-special characters in a string so the result renders
/// as literal text.
pub fn markdown_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
            | '!' | '|' | '>' | '<' => {
                out.push('\\');
                out.push(ch);
            },
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------

type Input = HashMap<String, serde_json::Value>;

fn string_field(input: &Input, key: &str) -> Option<String> {
    input.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

fn integer_field(input: &Input, key: &str) -> Option<u64> {
    input.get(key).and_then(|v| v.as_u64())
}

fn bool_field(input: &Input, key: &str) -> Option<bool> {
    input.get(key).and_then(|v| v.as_bool())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i + 3 >= max {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn lines_to_text(v: Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Array(lines)) => lines
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, input: serde_json::Value) -> ToolUse {
        let input_map: HashMap<String, serde_json::Value> = input
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        ToolUse {
            id: format!("toolu_{name}"),
            name: name.into(),
            input: input_map,
        }
    }

    #[test]
    fn bash_with_terminal_produces_terminal_content() {
        let t = tool("Bash", json!({"command": "ls /tmp"}));
        let info = tool_info_from_tool_use(&t, true, None);
        assert_eq!(info.kind, ToolKind::Execute);
        assert_eq!(info.title, "ls /tmp");
        assert_eq!(info.content.len(), 1);
        match &info.content[0] {
            ToolCallContent::Terminal { terminal_id } => {
                assert!(terminal_id.starts_with("toolu_"));
            },
            other => panic!("expected Terminal, got {other:?}"),
        }
    }

    #[test]
    fn bash_without_terminal_produces_text_block() {
        let t = tool("Bash", json!({"command": "ls /tmp"}));
        let info = tool_info_from_tool_use(&t, false, None);
        match &info.content[0] {
            ToolCallContent::Text { text } => assert!(text.contains("ls /tmp")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn read_adds_file_location() {
        let t = tool(
            "Read",
            json!({"file_path": "/src/main.rs", "offset": 10, "limit": 50}),
        );
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::Read);
        assert!(info.title.contains("/src/main.rs"));
        assert!(info.title.contains("10-"));
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.locations[0].line, Some(10));
    }

    #[test]
    fn write_produces_diff_with_no_old_text() {
        let t = tool(
            "Write",
            json!({"file_path": "/tmp/new.txt", "content": "hello"}),
        );
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::Edit);
        match &info.content[0] {
            ToolCallContent::Diff {
                path,
                old_text,
                new_text,
            } => {
                assert_eq!(path, Path::new("/tmp/new.txt"));
                assert!(old_text.is_none());
                assert_eq!(new_text, "hello");
            },
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn edit_produces_diff_with_old_and_new() {
        let t = tool(
            "Edit",
            json!({
                "file_path": "/tmp/x.rs",
                "old_string": "foo",
                "new_string": "bar"
            }),
        );
        let info = tool_info_from_tool_use(&t, false, None);
        match &info.content[0] {
            ToolCallContent::Diff {
                old_text, new_text, ..
            } => {
                assert_eq!(old_text.as_deref(), Some("foo"));
                assert_eq!(new_text, "bar");
            },
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn multi_edit_produces_one_diff_per_entry() {
        let t = tool(
            "MultiEdit",
            json!({
                "file_path": "/tmp/x.rs",
                "edits": [
                    {"old_string": "a", "new_string": "A"},
                    {"old_string": "b", "new_string": "B"}
                ]
            }),
        );
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.content.len(), 2);
    }

    #[test]
    fn glob_uses_pattern_in_title() {
        let t = tool("Glob", json!({"pattern": "**/*.rs"}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::Search);
        assert!(info.title.contains("**/*.rs"));
    }

    #[test]
    fn grep_builds_command_line_title() {
        let t = tool("Grep", json!({"pattern": "fn main", "-i": true, "-A": 5}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert!(info.title.contains("grep"));
        assert!(info.title.contains("-i"));
        assert!(info.title.contains("-A 5"));
        assert!(info.title.contains("fn main"));
    }

    #[test]
    fn todo_write_classified_as_think() {
        let t = tool("TodoWrite", json!({"todos": []}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::Think);
    }

    #[test]
    fn exit_plan_mode_kind_switch_mode() {
        let t = tool("ExitPlanMode", json!({"plan": "Do stuff"}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::SwitchMode);
        assert_eq!(info.title, "Ready to code?");
    }

    #[test]
    fn mcp_tool_uses_full_name_as_title() {
        let t = tool("mcp__weather__get_forecast", json!({"city": "SF"}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.title, "mcp__weather__get_forecast");
    }

    #[test]
    fn unknown_tool_falls_back_to_other() {
        let t = tool("MysteryTool", json!({"key": "value"}));
        let info = tool_info_from_tool_use(&t, false, None);
        assert_eq!(info.kind, ToolKind::Other);
        assert_eq!(info.title, "MysteryTool");
    }

    #[test]
    fn plan_entries_converts_todos() {
        let input = json!({
            "todos": [
                {"content": "Step 1", "status": "pending"},
                {"content": "Step 2", "status": "in_progress"},
                {"content": "Step 3", "status": "completed"},
            ]
        });
        let entries = plan_entries(&input);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, PlanEntryStatus::Pending);
        assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
        assert_eq!(entries[2].status, PlanEntryStatus::Completed);
        for entry in &entries {
            assert_eq!(entry.priority, "medium");
        }
    }

    #[test]
    fn plan_entries_skips_malformed_entries() {
        let input = json!({
            "todos": [
                {"content": "ok", "status": "pending"},
                {"content": "bad_status", "status": "frobnicated"},
                {"content": "missing_status"},
            ]
        });
        let entries = plan_entries(&input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "ok");
    }

    #[test]
    fn to_display_path_shortens_under_cwd() {
        let out = to_display_path(
            Path::new("/home/u/proj/src/main.rs"),
            Some(Path::new("/home/u/proj")),
        );
        assert_eq!(out, "src/main.rs");
    }

    #[test]
    fn to_display_path_falls_back_to_full_path() {
        let out = to_display_path(Path::new("/tmp/x.rs"), Some(Path::new("/home/u/proj")));
        assert_eq!(out, "/tmp/x.rs");
    }

    #[test]
    fn tool_update_from_tool_result_success() {
        let mut cache = HashMap::new();
        cache.insert(
            "t1".into(),
            ToolUseSnapshot {
                id: "t1".into(),
                name: "Read".into(),
                input: HashMap::new(),
            },
        );
        let update = tool_update_from_tool_result("t1", "file content", false, &cache, false);
        assert_eq!(update.status, Some(ToolCallStatus::Completed));
        match update.content.as_deref() {
            Some([ToolCallContent::Text { text }]) => assert_eq!(text, "file content"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn tool_update_bash_with_terminal_populates_terminal_meta() {
        let mut cache = HashMap::new();
        cache.insert(
            "bash_1".into(),
            ToolUseSnapshot {
                id: "bash_1".into(),
                name: "Bash".into(),
                input: HashMap::new(),
            },
        );
        let update = tool_update_from_tool_result("bash_1", "hello\n", false, &cache, true);
        let tm = update.terminal_meta.expect("terminal meta present");
        assert_eq!(tm.terminal_id, "bash_1");
        assert_eq!(tm.output.as_deref(), Some("hello\n"));
    }

    #[test]
    fn tool_update_error_status() {
        let cache = HashMap::new();
        let update = tool_update_from_tool_result("unknown", "oops", true, &cache, false);
        assert_eq!(update.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn markdown_escape_escapes_specials() {
        let out = markdown_escape("hello *world* `code`");
        assert_eq!(out, r"hello \*world\* \`code\`");
    }
}
