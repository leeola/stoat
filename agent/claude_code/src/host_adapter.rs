//! Adapter that exposes [`ClaudeCode`] via Stoat's [`ClaudeCodeHost`] trait.
//!
//! The wire protocol (`SdkMessage`) and the host trait (`AgentMessage`)
//! have different shapes: a single `SdkMessage::Assistant` can carry an
//! ordered list of content blocks that each become a separate
//! `AgentMessage`. The adapter expands wire messages into a pending
//! queue and drains the queue one at a time per `recv()` call.
//!
//! Unknown content types are surfaced as [`AgentMessage::Unknown`] so
//! schema drift in the Claude CLI never silently drops data.

use crate::{
    ClaudeCode,
    messages::{MessageContent, SdkMessage, UserContent, UserContentBlock, UserMessage},
};
use std::io;
use stoat::host::{AgentMessage, ClaudeCodeHost};

/// Expand one wire [`SdkMessage`] into zero or more [`AgentMessage`]s.
///
/// Returns `Vec` because a single assistant message carries a
/// `Vec<MessageContent>`: text blocks, tool uses, and unknowns all
/// become distinct `AgentMessage`s in source order.
pub(crate) fn sdk_message_to_agent_messages(msg: SdkMessage) -> Vec<AgentMessage> {
    match msg {
        SdkMessage::System {
            session_id,
            model,
            tools,
            ..
        } => vec![AgentMessage::Init {
            session_id,
            model,
            tools,
        }],

        SdkMessage::Assistant { message, .. } => message
            .content
            .into_iter()
            .map(|content| match content {
                MessageContent::Text { text } => AgentMessage::Text { text },
                MessageContent::ToolUse { id, name, input } => AgentMessage::ToolUse {
                    id,
                    name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                },
                MessageContent::Unknown(value) => AgentMessage::Unknown {
                    raw: value.to_string(),
                },
            })
            .collect(),

        SdkMessage::User { message, .. } => extract_tool_results(message),

        SdkMessage::Result {
            total_cost_usd,
            duration_ms,
            num_turns,
            is_error,
            result,
            ..
        } => {
            if is_error {
                vec![AgentMessage::Error {
                    message: result.unwrap_or_else(|| "claude run failed".into()),
                }]
            } else {
                vec![AgentMessage::Result {
                    cost_usd: total_cost_usd,
                    duration_ms,
                    num_turns,
                }]
            }
        },
    }
}

/// Extract `AgentMessage::ToolResult` entries from a wire user message.
///
/// Plain text user messages are echoes of what Stoat already sent, and
/// stray text blocks inside structured content are not useful to the
/// host layer, so only `UserContentBlock::ToolResult` blocks produce
/// output.
fn extract_tool_results(message: UserMessage) -> Vec<AgentMessage> {
    match message.content {
        UserContent::Text(_) => Vec::new(),
        UserContent::Blocks(blocks) => blocks
            .into_iter()
            .filter_map(|block| match block {
                UserContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => Some(AgentMessage::ToolResult {
                    id: tool_use_id,
                    content,
                }),
                UserContentBlock::Text { .. } => None,
            })
            .collect(),
    }
}

impl ClaudeCodeHost for ClaudeCode {
    async fn send(&self, content: &str) -> io::Result<()> {
        self.send_message(content).await.map_err(io::Error::other)
    }

    async fn recv(&self) -> Option<AgentMessage> {
        loop {
            // Drain any already-buffered expansion first.
            if let Some(msg) = self.pending.lock().unwrap().pop_front() {
                return Some(msg);
            }

            // Pull the next wire message. Lock lives only across this
            // single `.await`; the sync `pending` mutex is never held
            // across it.
            let sdk_msg = {
                let mut rx = self.process_stdout_rx.lock().await;
                rx.recv().await?
            };

            // Expand and enqueue. The loop pops one on the next pass.
            let expanded = sdk_message_to_agent_messages(sdk_msg);
            self.pending.lock().unwrap().extend(expanded);
        }
    }

    fn is_alive(&self) -> bool {
        self.is_alive_inner()
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.shutdown_inner().await.map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{
        ApiKeySource, AssistantMessage, PermissionMode, ResultSubtype, Role, SystemSubtype,
    };
    use std::collections::HashMap;

    fn assistant(content: Vec<MessageContent>) -> SdkMessage {
        SdkMessage::Assistant {
            message: AssistantMessage {
                role: Role::Assistant,
                content,
            },
            session_id: "sess".into(),
        }
    }

    fn system_init() -> SdkMessage {
        SdkMessage::System {
            subtype: SystemSubtype::Init,
            api_key_source: ApiKeySource::None,
            cwd: "/tmp".into(),
            session_id: "sess-42".into(),
            tools: vec!["Read".into(), "Write".into()],
            mcp_servers: vec![],
            model: "claude-opus".into(),
            permission_mode: PermissionMode::Default,
        }
    }

    #[test]
    fn sdk_system_maps_to_init() {
        let out = sdk_message_to_agent_messages(system_init());
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Init {
                session_id,
                model,
                tools,
            } => {
                assert_eq!(session_id, "sess-42");
                assert_eq!(model, "claude-opus");
                assert_eq!(tools, &vec!["Read".to_string(), "Write".to_string()]);
            },
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_text_maps_to_text() {
        let out = sdk_message_to_agent_messages(assistant(vec![MessageContent::Text {
            text: "hello".into(),
        }]));
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_multi_content_expands() {
        let mut input = HashMap::new();
        input.insert("path".to_string(), serde_json::json!("/x"));
        let out = sdk_message_to_agent_messages(assistant(vec![
            MessageContent::Text { text: "a".into() },
            MessageContent::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input,
            },
            MessageContent::Unknown(serde_json::json!({"type": "image", "data": "xyz"})),
        ]));
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], AgentMessage::Text { text } if text == "a"));
        assert!(matches!(&out[1], AgentMessage::ToolUse { id, .. } if id == "t1"));
        assert!(matches!(&out[2], AgentMessage::Unknown { .. }));
    }

    #[test]
    fn sdk_assistant_tool_use_serializes_input() {
        let mut input = HashMap::new();
        input.insert("path".to_string(), serde_json::json!("/tmp/f"));
        input.insert("mode".to_string(), serde_json::json!(420));
        let out = sdk_message_to_agent_messages(assistant(vec![MessageContent::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input,
        }]));
        match &out[0] {
            AgentMessage::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "Read");
                let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
                assert_eq!(parsed["path"], "/tmp/f");
                assert_eq!(parsed["mode"], 420);
            },
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_unknown_content_preserves_raw() {
        let value = serde_json::json!({"type": "image", "source": {"data": "xyz"}});
        let out =
            sdk_message_to_agent_messages(assistant(vec![MessageContent::Unknown(value.clone())]));
        match &out[0] {
            AgentMessage::Unknown { raw } => {
                let reparsed: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(reparsed, value);
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn sdk_user_tool_result_maps_to_tool_result() {
        let msg = SdkMessage::User {
            message: UserMessage::from_tool_result("tool_7", "file contents"),
            session_id: "sess".into(),
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::ToolResult { id, content } => {
                assert_eq!(id, "tool_7");
                assert_eq!(content, "file contents");
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn sdk_user_plain_text_is_dropped() {
        let msg = SdkMessage::User {
            message: UserMessage::from_text("hi claude"),
            session_id: "sess".into(),
        };
        let out = sdk_message_to_agent_messages(msg);
        assert!(
            out.is_empty(),
            "plain text user echoes should produce nothing, got {out:?}"
        );
    }

    #[test]
    fn sdk_result_success_maps_to_result() {
        let msg = SdkMessage::Result {
            subtype: ResultSubtype::Success,
            duration_ms: 1234,
            duration_api_ms: 1000,
            is_error: false,
            num_turns: 3,
            result: Some("done".into()),
            session_id: "sess".into(),
            total_cost_usd: 0.02,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Result {
                cost_usd,
                duration_ms,
                num_turns,
            } => {
                assert_eq!(*cost_usd, 0.02);
                assert_eq!(*duration_ms, 1234);
                assert_eq!(*num_turns, 3);
            },
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn sdk_result_error_maps_to_error() {
        let msg = SdkMessage::Result {
            subtype: ResultSubtype::ErrorDuringExecution,
            duration_ms: 50,
            duration_api_ms: 40,
            is_error: true,
            num_turns: 1,
            result: Some("boom".into()),
            session_id: "sess".into(),
            total_cost_usd: 0.0,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Error { message } => assert_eq!(message, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
