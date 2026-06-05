//! Demux of the ACP `session/update` streaming notification into the
//! host-facing [`AgentMessage`].
//!
//! The `update` object is kept as a [`Value`] and matched on its
//! `sessionUpdate` discriminator so unmodeled variants and fields are
//! tolerated rather than failing the whole stream. Each field is read
//! defensively; a malformed variant maps to `None` (dropped) rather than
//! panicking.

use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use stoat::host::{
    AgentMessage, PlanEntry, PlanEntryStatus, TokenUsage, ToolCallContent, ToolCallLocation,
    ToolCallStatus, ToolKind,
};

pub(crate) const SESSION_UPDATE: &str = "session/update";

/// `session/update` params: the session the update belongs to and the
/// raw update object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionUpdateNotification {
    pub session_id: String,
    pub update: Value,
}

/// Map one `session/update` `update` onto an [`AgentMessage`], or `None`
/// for updates that carry no host-facing event: a user-message echo, an
/// unmodeled variant, or a malformed/empty payload.
pub(crate) fn demux_session_update(update: &Value) -> Option<AgentMessage> {
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => Some(AgentMessage::Text {
            text: text_content(update)?,
        }),
        "agent_thought_chunk" => Some(AgentMessage::Thinking {
            text: text_content(update)?,
            signature: String::new(),
        }),
        "user_message_chunk" => None,
        "tool_call" => tool_call(update),
        "tool_call_update" => tool_call_update(update),
        "plan" => Some(AgentMessage::Plan {
            entries: plan_entries(update),
        }),
        "current_mode_update" => Some(AgentMessage::ModeChanged {
            mode: update.get("currentModeId")?.as_str()?.to_string(),
        }),
        "usage_update" => Some(AgentMessage::Usage {
            accumulated: TokenUsage {
                output_tokens: update.get("used")?.as_u64()?,
                ..Default::default()
            },
            last: TokenUsage::default(),
        }),
        _ => None,
    }
}

/// Extract the text of a chunk update's `content` content block, or
/// `None` when the block is not a text block.
fn text_content(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if content.get("type")?.as_str()? == "text" {
        content.get("text")?.as_str().map(str::to_string)
    } else {
        None
    }
}

fn tool_call(update: &Value) -> Option<AgentMessage> {
    Some(AgentMessage::ToolUse {
        id: update.get("toolCallId")?.as_str()?.to_string(),
        name: string_field(update, "kind"),
        input: update
            .get("rawInput")
            .map(ToString::to_string)
            .unwrap_or_default(),
        kind: tool_kind(update.get("kind").and_then(Value::as_str)),
        title: string_field(update, "title"),
        content: tool_content(update.get("content")),
        locations: tool_locations(update.get("locations")),
    })
}

fn tool_call_update(update: &Value) -> Option<AgentMessage> {
    Some(AgentMessage::ToolUpdate {
        id: update.get("toolCallId")?.as_str()?.to_string(),
        content: tool_content(update.get("content")),
        status: tool_status(update.get("status").and_then(Value::as_str)),
    })
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn tool_kind(kind: Option<&str>) -> ToolKind {
    match kind {
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("execute") => ToolKind::Execute,
        Some("search") => ToolKind::Search,
        Some("fetch") => ToolKind::Fetch,
        Some("think") => ToolKind::Think,
        Some("switch_mode") => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

/// ACP omits `status` on incremental tool-call updates; absence means
/// the call is still running, so default to in-progress.
fn tool_status(status: Option<&str>) -> ToolCallStatus {
    match status {
        Some("pending") => ToolCallStatus::Pending,
        Some("completed") => ToolCallStatus::Completed,
        Some("failed") => ToolCallStatus::Failed,
        Some("cancelled") => ToolCallStatus::Cancelled,
        _ => ToolCallStatus::InProgress,
    }
}

fn tool_content(content: Option<&Value>) -> Vec<ToolCallContent> {
    content
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(tool_content_block).collect())
        .unwrap_or_default()
}

fn tool_content_block(block: &Value) -> Option<ToolCallContent> {
    match block.get("type")?.as_str()? {
        "text" => Some(ToolCallContent::Text {
            text: block.get("text")?.as_str()?.to_string(),
        }),
        // The agent may wrap a content block in `{type:"content", content}`.
        "content" => Some(ToolCallContent::Text {
            text: block.get("content")?.get("text")?.as_str()?.to_string(),
        }),
        "diff" => Some(ToolCallContent::Diff {
            path: PathBuf::from(block.get("path")?.as_str()?),
            old_text: block
                .get("oldText")
                .and_then(Value::as_str)
                .map(str::to_string),
            new_text: string_field(block, "newText"),
        }),
        "terminal" => Some(ToolCallContent::Terminal {
            terminal_id: block.get("terminalId")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

fn tool_locations(locations: Option<&Value>) -> Vec<ToolCallLocation> {
    locations
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|loc| {
                    Some(ToolCallLocation {
                        path: PathBuf::from(loc.get("path")?.as_str()?),
                        line: loc.get("line").and_then(Value::as_u64).map(|n| n as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn plan_entries(update: &Value) -> Vec<PlanEntry> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(plan_entry).collect())
        .unwrap_or_default()
}

fn plan_entry(entry: &Value) -> Option<PlanEntry> {
    let content = entry.get("content")?;
    let content = content.as_str().map(str::to_string).or_else(|| {
        content
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
    })?;
    Some(PlanEntry {
        content,
        status: plan_status(entry.get("status").and_then(Value::as_str)),
        priority: entry
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("medium")
            .to_string(),
    })
}

fn plan_status(status: Option<&str>) -> PlanEntryStatus {
    match status {
        Some("in_progress") => PlanEntryStatus::InProgress,
        Some("completed") => PlanEntryStatus::Completed,
        _ => PlanEntryStatus::Pending,
    }
}
