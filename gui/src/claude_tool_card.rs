//! Render a `ChatMessageContent::ToolUse` entry as a collapsible
//! tool card. Header click toggles the card's expansion state via
//! [`crate::claude_chat::ClaudeChat::toggle_expanded`]; the
//! `Tab`-driven keyboard path lives in `ClaudeChat`'s
//! focus-cycling methods and dispatches the same toggle through
//! [`crate::claude_chat::ClaudeChat::toggle_focused_expansion`].
//!
//! Mirrors the TUI rendering at
//! `stoat/src/render/claude_pane.rs` for the tool-card layer:
//! status badge, per-tool header extraction, and the
//! collapsed-preview / expanded-input-and-output body split.

use crate::{claude_chat::ClaudeChat, theme::ActiveTheme};
use gpui::{
    div, AnyElement, Context, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled,
};
use std::collections::HashSet;
use stoat::{
    claude_chat::{ChatMessage, ChatMessageContent, ToolCardStatus},
    host::ToolCallStatus,
};

/// Render-time classification of a tool card mirroring the TUI
/// helper at `stoat/src/claude_chat.rs::tool_card_status`. The
/// cancelled set takes precedence over server status; absent
/// results imply the tool is still running.
pub(crate) fn tool_card_status(
    messages: &[ChatMessage],
    cancelled: &HashSet<String>,
    tool_id: &str,
) -> ToolCardStatus {
    if cancelled.contains(tool_id) {
        return ToolCardStatus::Cancelled;
    }
    for msg in messages {
        if let ChatMessageContent::ToolResult { id, status, .. } = &msg.content {
            if id == tool_id {
                return match status {
                    ToolCallStatus::Failed => ToolCardStatus::Failed,
                    _ => ToolCardStatus::Done,
                };
            }
        }
    }
    ToolCardStatus::Running
}

/// Render the tool card for a `ToolUse` message with `id`, `name`,
/// and raw JSON `input`. Pulls expansion + focus state out of
/// `chat`, computes the status from the surrounding scrollback, and
/// wires the header click listener to
/// [`ClaudeChat::toggle_expanded`].
pub(crate) fn render_tool_card(
    chat: &ClaudeChat,
    id: &str,
    name: &str,
    input: &str,
    cx: &mut Context<'_, ClaudeChat>,
) -> AnyElement {
    let status = tool_card_status(&chat.messages, &chat.cancelled_tool_uses, id);
    let is_focused = chat.focused_tool_id.as_deref() == Some(id);
    let is_expanded = chat.expanded_tool_ids.contains(id);
    let result_content = find_result_content(&chat.messages, id);

    let theme = cx.theme();
    let header_color = if is_focused {
        theme.chat_tool_focused
    } else {
        theme.chat_tool_header
    };
    let body_color = theme.chat_tool_body;
    let status_color = status_color(status, &theme);
    let focused_color = theme.chat_tool_focused;

    let header_text = format!("\u{23fa} {}", format_tool_header(name, input));
    let status_text = format!("  {}", status_label(status));
    let element_id: ElementId = SharedString::from(format!("claude_tool_card:{id}")).into();

    let id_for_listener = id.to_string();
    let header = div()
        .id(element_id)
        .px_2()
        .py_1()
        .text_color(header_color)
        .child(SharedString::from(header_text))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.toggle_expanded(&id_for_listener, cx);
        }));

    let mut card = div().flex().flex_col().w_full().child(header).child(
        div()
            .px_2()
            .text_color(status_color)
            .child(SharedString::from(status_text)),
    );

    if is_focused {
        card = card.child(
            div()
                .px_2()
                .text_color(focused_color)
                .child(SharedString::from("  (focused)")),
        );
    }

    if is_expanded {
        card = card.child(
            div()
                .px_2()
                .text_color(body_color)
                .child(SharedString::from(format!(
                    "  input: {}",
                    format_tool_input(input)
                ))),
        );
        if let Some(content) = result_content {
            card = card.child(
                div()
                    .px_2()
                    .text_color(body_color)
                    .child(SharedString::from("  output:")),
            );
            for line in content.lines() {
                card = card.child(
                    div()
                        .px_2()
                        .text_color(body_color)
                        .child(SharedString::from(format!("     {line}"))),
                );
            }
        }
    } else if let Some(content) = result_content {
        card = card.child(
            div()
                .px_2()
                .text_color(body_color)
                .child(SharedString::from(format!(
                    "  {}",
                    format_tool_result_preview(content)
                ))),
        );
    }

    card.into_any_element()
}

fn status_color(status: ToolCardStatus, theme: &crate::theme::ThemeColors) -> Hsla {
    match status {
        ToolCardStatus::Running => theme.chat_tool_status_running,
        ToolCardStatus::Done => theme.chat_tool_status_done,
        ToolCardStatus::Failed => theme.chat_tool_status_failed,
        ToolCardStatus::Cancelled => theme.chat_tool_status_cancelled,
    }
}

/// Per-tool extraction of a one-line summary from the tool's JSON
/// input. Falls back to `name(first_key=first_value)` when the
/// schema does not match one of the well-known cases, then to
/// `name(...)` when the input is not parseable JSON.
pub(crate) fn format_tool_header(name: &str, input_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(input_json);
    let Ok(value) = parsed else {
        return format!("{name}(...)");
    };
    let obj = value.as_object();

    match name {
        "Bash" => {
            if let Some(cmd) = obj.and_then(|o| o.get("command")).and_then(|v| v.as_str()) {
                return format!("Bash({})", truncate(cmd, 60));
            }
        },
        "Read" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                let offset = obj.and_then(|o| o.get("offset")).and_then(|v| v.as_u64());
                let limit = obj.and_then(|o| o.get("limit")).and_then(|v| v.as_u64());
                return match (offset, limit) {
                    (Some(start), Some(len)) if len > 0 => {
                        format!("Read({}:{}-{})", short_path(p), start, start + len - 1)
                    },
                    (Some(start), _) => format!("Read({}:{})", short_path(p), start),
                    _ => format!("Read({})", short_path(p)),
                };
            }
        },
        "Edit" | "Write" | "NotebookEdit" => {
            if let Some(p) = obj
                .and_then(|o| o.get("file_path"))
                .and_then(|v| v.as_str())
            {
                return format!("{name}({})", short_path(p));
            }
        },
        "Grep" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Grep({})", truncate(p, 60));
            }
        },
        "Glob" => {
            if let Some(p) = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()) {
                return format!("Glob({})", truncate(p, 60));
            }
        },
        _ => {},
    }

    if let Some(o) = obj {
        if let Some((k, v)) = o.iter().next() {
            let vs = match v {
                serde_json::Value::String(s) => truncate(s, 60),
                other => truncate(&other.to_string(), 60),
            };
            return format!("{name}({k}={vs})");
        }
    }
    format!("{name}(...)")
}

/// First-line preview of a tool result. Multi-line results gain a
/// `(+N more lines)` suffix so the collapsed card hints at the
/// truncated tail.
pub(crate) fn format_tool_result_preview(content: &str) -> String {
    let first = content.lines().next().unwrap_or("");
    let total_lines = content.lines().count();
    let preview = truncate(first, 80);
    if total_lines > 1 {
        format!("{preview} (+{} more lines)", total_lines - 1)
    } else {
        preview
    }
}

fn format_tool_input(input_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(input_json);
    match parsed {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| input_json.to_string()),
        Err(_) => input_json.to_string(),
    }
}

fn find_result_content<'a>(messages: &'a [ChatMessage], tool_id: &str) -> Option<&'a str> {
    messages.iter().find_map(|m| match &m.content {
        ChatMessageContent::ToolResult { id, content, .. } if id == tool_id => {
            Some(content.as_str())
        },
        _ => None,
    })
}

fn status_label(status: ToolCardStatus) -> &'static str {
    match status {
        ToolCardStatus::Running => "running",
        ToolCardStatus::Done => "done",
        ToolCardStatus::Failed => "failed",
        ToolCardStatus::Cancelled => "cancelled",
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        format!(
            "{}...",
            s.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => p.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};

    fn tool_result(id: &str, status: ToolCallStatus) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: ChatMessageContent::ToolResult {
                id: id.into(),
                content: String::new(),
                status,
            },
            checkpoint_sha: None,
        }
    }

    #[test]
    fn format_tool_header_extracts_bash_command() {
        let header = format_tool_header("Bash", r#"{"command":"ls -la"}"#);
        assert_eq!(header, "Bash(ls -la)");
    }

    #[test]
    fn format_tool_header_extracts_read_file_path_with_offset_limit() {
        let header = format_tool_header(
            "Read",
            r#"{"file_path":"/repo/src/lib.rs","offset":10,"limit":5}"#,
        );
        assert_eq!(header, "Read(src/lib.rs:10-14)");
    }

    #[test]
    fn format_tool_header_falls_back_to_first_kv() {
        let header = format_tool_header("Unknown", r#"{"x":"y"}"#);
        assert_eq!(header, "Unknown(x=y)");
    }

    #[test]
    fn format_tool_header_falls_back_to_paren_on_bad_json() {
        let header = format_tool_header("Bash", "not json");
        assert_eq!(header, "Bash(...)");
    }

    #[test]
    fn format_tool_result_preview_single_line() {
        assert_eq!(format_tool_result_preview("hello"), "hello");
    }

    #[test]
    fn format_tool_result_preview_multi_line_appends_count() {
        let content = "first\nsecond\nthird\n";
        assert_eq!(format_tool_result_preview(content), "first (+2 more lines)");
    }

    #[test]
    fn tool_card_status_returns_running_without_result() {
        let cancelled = HashSet::new();
        assert_eq!(
            tool_card_status(&[], &cancelled, "toolu_x"),
            ToolCardStatus::Running
        );
    }

    #[test]
    fn tool_card_status_returns_done_for_completed_result() {
        let messages = vec![tool_result("toolu_x", ToolCallStatus::Completed)];
        let cancelled = HashSet::new();
        assert_eq!(
            tool_card_status(&messages, &cancelled, "toolu_x"),
            ToolCardStatus::Done
        );
    }

    #[test]
    fn tool_card_status_returns_failed_for_failed_result() {
        let messages = vec![tool_result("toolu_x", ToolCallStatus::Failed)];
        let cancelled = HashSet::new();
        assert_eq!(
            tool_card_status(&messages, &cancelled, "toolu_x"),
            ToolCardStatus::Failed
        );
    }

    #[test]
    fn tool_card_status_returns_cancelled_when_in_cancelled_set() {
        let messages = vec![tool_result("toolu_x", ToolCallStatus::Completed)];
        let mut cancelled = HashSet::new();
        cancelled.insert("toolu_x".into());
        assert_eq!(
            tool_card_status(&messages, &cancelled, "toolu_x"),
            ToolCardStatus::Cancelled,
            "cancelled takes precedence over completed status",
        );
    }
}
