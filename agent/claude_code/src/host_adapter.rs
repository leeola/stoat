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
use async_trait::async_trait;
use std::io;
use stoat::host::{AgentMessage, ClaudeCodeHost};

/// Expand one wire [`SdkMessage`] into zero or more [`AgentMessage`]s.
///
/// Returns `Vec` because a single assistant message carries a
/// `Vec<MessageContent>`: text blocks, tool uses, and unknowns all
/// become distinct `AgentMessage`s in source order.
pub(crate) fn sdk_message_to_agent_messages(msg: SdkMessage) -> Vec<AgentMessage> {
    let text_delta = msg.as_text_delta().map(str::to_owned);
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
                MessageContent::Thinking {
                    thinking,
                    signature,
                } => AgentMessage::Thinking {
                    text: thinking,
                    signature,
                },
                MessageContent::RedactedThinking { data } => AgentMessage::Unknown {
                    raw: serde_json::json!({
                        "type": "redacted_thinking",
                        "data": data,
                    })
                    .to_string(),
                },
                MessageContent::ServerToolUse { id, name, input } => AgentMessage::ServerToolUse {
                    id,
                    name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                },
                MessageContent::ServerToolResult {
                    tool_use_id,
                    content,
                } => AgentMessage::ServerToolResult {
                    id: tool_use_id,
                    content: match content {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    },
                },
                MessageContent::Unknown(value) => AgentMessage::Unknown {
                    raw: value.to_string(),
                },
            })
            .collect(),

        SdkMessage::User { message, .. } => extract_tool_results(message),

        SdkMessage::StreamEvent { event, .. } => match text_delta {
            Some(text) => vec![AgentMessage::PartialText { text }],
            None => vec![AgentMessage::Unknown {
                raw: event.to_string(),
            }],
        },

        // Control requests are normally intercepted by the permission
        // dispatcher before they reach the host queue. If one arrives
        // here, no dispatcher is active and the host is expected to
        // treat it as an unrecognized message.
        SdkMessage::ControlRequest {
            request_id,
            request,
        } => vec![AgentMessage::Unknown {
            raw: serde_json::json!({
                "type": "control_request",
                "request_id": request_id,
                "request": request,
            })
            .to_string(),
        }],

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
                    ..
                } => Some(AgentMessage::ToolResult {
                    id: tool_use_id,
                    content: content.as_text(),
                }),
                UserContentBlock::Text { .. } => None,
            })
            .collect(),
    }
}

#[async_trait]
impl ClaudeCodeHost for ClaudeCode {
    async fn send(&self, content: &str) -> io::Result<()> {
        self.send_message(content).await.map_err(io::Error::other)
    }

    async fn recv(&self) -> Option<AgentMessage> {
        loop {
            // Drain any already-buffered expansion first.
            if let Some(msg) = self
                .pending
                .lock()
                .expect("ClaudeCode pending mutex poisoned")
                .pop_front()
            {
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
            self.pending
                .lock()
                .expect("ClaudeCode pending mutex poisoned")
                .extend(expanded);
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

    #[test]
    fn sdk_assistant_thinking_maps_to_thinking() {
        let out = sdk_message_to_agent_messages(assistant(vec![MessageContent::Thinking {
            thinking: "let me ponder".into(),
            signature: "sig-xyz".into(),
        }]));
        match &out[0] {
            AgentMessage::Thinking { text, signature } => {
                assert_eq!(text, "let me ponder");
                assert_eq!(signature, "sig-xyz");
            },
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_redacted_thinking_falls_back_to_unknown() {
        let out =
            sdk_message_to_agent_messages(assistant(vec![MessageContent::RedactedThinking {
                data: "encrypted-xyz".into(),
            }]));
        match &out[0] {
            AgentMessage::Unknown { raw } => {
                let reparsed: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(reparsed["type"], "redacted_thinking");
                assert_eq!(reparsed["data"], "encrypted-xyz");
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_server_tool_use_maps() {
        let mut input = HashMap::new();
        input.insert("query".to_string(), serde_json::json!("rust async"));
        let out = sdk_message_to_agent_messages(assistant(vec![MessageContent::ServerToolUse {
            id: "srvtoolu_1".into(),
            name: "web_search".into(),
            input,
        }]));
        match &out[0] {
            AgentMessage::ServerToolUse { id, name, input } => {
                assert_eq!(id, "srvtoolu_1");
                assert_eq!(name, "web_search");
                let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
                assert_eq!(parsed["query"], "rust async");
            },
            other => panic!("expected ServerToolUse, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_server_tool_result_maps() {
        let out =
            sdk_message_to_agent_messages(assistant(vec![MessageContent::ServerToolResult {
                tool_use_id: "srvtoolu_1".into(),
                content: serde_json::json!("10 results found"),
            }]));
        match &out[0] {
            AgentMessage::ServerToolResult { id, content } => {
                assert_eq!(id, "srvtoolu_1");
                assert_eq!(content, "10 results found");
            },
            other => panic!("expected ServerToolResult, got {other:?}"),
        }
    }

    #[test]
    fn sdk_assistant_server_tool_result_structured_content_serializes() {
        let out =
            sdk_message_to_agent_messages(assistant(vec![MessageContent::ServerToolResult {
                tool_use_id: "srvtoolu_2".into(),
                content: serde_json::json!([{"type": "text", "text": "result 1"}]),
            }]));
        match &out[0] {
            AgentMessage::ServerToolResult { id, content } => {
                assert_eq!(id, "srvtoolu_2");
                let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
                assert_eq!(parsed[0]["text"], "result 1");
            },
            other => panic!("expected ServerToolResult, got {other:?}"),
        }
    }

    #[test]
    fn sdk_stream_event_text_delta_maps_to_partial_text() {
        let msg = SdkMessage::StreamEvent {
            event: serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello "}
            }),
            session_id: "sess-s".into(),
            parent_tool_use_id: None,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::PartialText { text } => assert_eq!(text, "hello "),
            other => panic!("expected PartialText, got {other:?}"),
        }
    }

    #[test]
    fn sdk_stream_event_non_text_delta_maps_to_unknown() {
        let msg = SdkMessage::StreamEvent {
            event: serde_json::json!({
                "type": "message_start",
                "message": {"id": "m1"}
            }),
            session_id: "sess-s".into(),
            parent_tool_use_id: None,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Unknown { raw } => {
                let reparsed: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(reparsed["type"], "message_start");
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
