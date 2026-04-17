//! Adapter that exposes [`ClaudeCode`] via Stoat's [`ClaudeCodeSession`] trait.
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
    messages::{
        AssistantMessage, MessageContent, SdkMessage, SystemSubtype, UserContent, UserContentBlock,
        UserMessage,
    },
    tools::{ToolUseSnapshot, tool_info_from_tool_use, tool_update_from_tool_result},
};
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
};
use stoat::host::{
    AgentMessage, ClaudeCodeSession, HookLifecycleEvent, SessionStateEvent, TaskEvent, TokenUsage,
    ToolCallStatus, ToolKind,
};

/// Per-session state threaded through the adapter so the classifier
/// can reuse its previous tool-use snapshots (for matching `tool_result`
/// blocks to the original tool name, Bash terminal-id bookkeeping,
/// etc.).
#[derive(Default)]
pub(crate) struct AdapterState {
    pub tool_use_cache: HashMap<String, ToolUseSnapshot>,
    pub accumulated_usage: TokenUsage,
    pub supports_terminal: bool,
    pub cwd: Option<PathBuf>,
    /// Maps a streaming `content_block.index` to the tool_use id the
    /// CLI announced in the preceding `content_block_start`. Lets
    /// `input_json_delta` events correlate back to the tool id the
    /// consumer is rendering.
    pub streaming_tool_ids: HashMap<u64, String>,
    /// Indexes of content blocks whose `text` was already streamed via
    /// `content_block_delta`. The adapter drops the equivalent `Text`
    /// block when the full `Assistant` message arrives so consumers
    /// don't render the same turn twice.
    pub streamed_text_indexes: HashSet<u64>,
    /// Same dedup tracking for `thinking_delta` streams vs the final
    /// `Thinking` block.
    pub streamed_thinking_indexes: HashSet<u64>,
    /// Accumulated streamed text per block index. Used to match
    /// against the full-message Text blocks (and to diagnose races).
    pub streaming_text_buffers: HashMap<u64, String>,
    /// Shared prompt-state; `Some` when the adapter is driven by a
    /// live `ClaudeCode` session. Used to drop CLI echoes of our own
    /// user messages when `replay-user-messages` is enabled.
    pub prompt_state: Option<std::sync::Arc<std::sync::Mutex<crate::claude_code::PromptState>>>,
}

impl AdapterState {
    /// Reset all streaming state. Invoked on `message_stop` or
    /// whenever a turn boundary is observed so the next turn's
    /// `content_block_index` values start from a clean slate.
    pub fn reset_streaming_state(&mut self) {
        self.streaming_tool_ids.clear();
        self.streamed_text_indexes.clear();
        self.streamed_thinking_indexes.clear();
        self.streaming_text_buffers.clear();
    }
}

/// Expand one wire [`SdkMessage`] into zero or more [`AgentMessage`]s.
///
/// A single `Assistant` message carries a `Vec<MessageContent>`: text
/// blocks, tool uses, and unknowns all become distinct `AgentMessage`s
/// in source order. `state` is mutated in place so the classifier can
/// remember tool uses for later `tool_result` correlation and so token
/// usage accumulates across turns.
pub(crate) fn sdk_message_to_agent_messages(
    msg: SdkMessage,
    state: &mut AdapterState,
) -> Vec<AgentMessage> {
    // Stream events still surface as raw text / JSON deltas. We handle
    // them first so the assistant branch below doesn't have to.
    if matches!(msg, SdkMessage::StreamEvent { .. }) {
        return stream_event_to_messages(msg, state);
    }

    match msg {
        SdkMessage::System {
            subtype: SystemSubtype::Init,
            session_id,
            model,
            tools,
            ..
        } => vec![AgentMessage::Init {
            session_id,
            model: model.unwrap_or_default(),
            tools: tools.unwrap_or_default(),
        }],

        SdkMessage::System {
            subtype: SystemSubtype::Status,
            status,
            text,
            ..
        } => vec![AgentMessage::SessionState(SessionStateEvent::Status {
            text: status.unwrap_or_else(|| text.unwrap_or_default()),
        })],

        SdkMessage::System {
            subtype: SystemSubtype::SessionStateChanged,
            state: session_state,
            ..
        } => vec![AgentMessage::SessionState(
            SessionStateEvent::StateChanged {
                state: session_state.unwrap_or_default(),
            },
        )],

        SdkMessage::System {
            subtype: SystemSubtype::CompactBoundary,
            trigger,
            pre_tokens,
            post_tokens,
            ..
        } => vec![AgentMessage::SessionState(
            SessionStateEvent::CompactBoundary {
                trigger,
                pre_tokens,
                post_tokens,
            },
        )],

        SdkMessage::System {
            subtype: SystemSubtype::LocalCommandOutput,
            text,
            ..
        } => vec![AgentMessage::SessionState(
            SessionStateEvent::LocalCommandOutput {
                text: text.unwrap_or_default(),
            },
        )],

        SdkMessage::System {
            subtype: SystemSubtype::ApiRetry,
            attempt,
            reason,
            ..
        } => vec![AgentMessage::SessionState(SessionStateEvent::ApiRetry {
            attempt,
            reason,
        })],

        SdkMessage::System {
            subtype: SystemSubtype::FilesPersisted,
            paths,
            ..
        } => vec![AgentMessage::FilesPersisted {
            paths: paths
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        }],

        SdkMessage::System {
            subtype: SystemSubtype::TaskStarted,
            task_id,
            parent_tool_use_id,
            title,
            ..
        } => vec![AgentMessage::TaskEvent(TaskEvent::Started {
            task_id,
            parent_tool_use_id,
            title,
        })],

        SdkMessage::System {
            subtype: SystemSubtype::TaskNotification,
            task_id,
            text,
            ..
        } => vec![AgentMessage::TaskEvent(TaskEvent::Notification {
            task_id,
            text: text.unwrap_or_default(),
        })],

        SdkMessage::System {
            subtype: SystemSubtype::TaskProgress,
            task_id,
            ..
        } => vec![AgentMessage::TaskEvent(TaskEvent::Progress { task_id })],

        SdkMessage::System {
            subtype: SystemSubtype::TaskUpdated,
            task_id,
            ..
        } => vec![AgentMessage::TaskEvent(TaskEvent::Updated { task_id })],

        SdkMessage::System {
            subtype: SystemSubtype::HookStarted,
            hook_event_name,
            extra,
            ..
        } => vec![AgentMessage::Hook(HookLifecycleEvent::Started {
            hook_event_name,
            payload_json: serde_json::Value::Object(extra).to_string(),
        })],

        SdkMessage::System {
            subtype: SystemSubtype::HookProgress,
            hook_event_name,
            extra,
            ..
        } => vec![AgentMessage::Hook(HookLifecycleEvent::Progress {
            hook_event_name,
            payload_json: serde_json::Value::Object(extra).to_string(),
        })],

        SdkMessage::System {
            subtype: SystemSubtype::HookResponse,
            hook_event_name,
            extra,
            ..
        } => vec![AgentMessage::Hook(HookLifecycleEvent::Response {
            hook_event_name,
            payload_json: serde_json::Value::Object(extra).to_string(),
        })],

        SdkMessage::System {
            subtype: SystemSubtype::ElicitationComplete,
            extra,
            ..
        } => {
            let obj = serde_json::Value::Object(extra);
            let id = obj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            vec![AgentMessage::ElicitationComplete {
                id,
                outcome_json: obj.to_string(),
            }]
        },

        SdkMessage::System {
            subtype: SystemSubtype::Unknown(name),
            extra,
            ..
        } => vec![AgentMessage::Unknown {
            raw: serde_json::json!({ "subtype": name, "payload": extra }).to_string(),
        }],

        SdkMessage::Assistant { message, .. } => assistant_to_messages(message, state),

        SdkMessage::User {
            message,
            message_uuid,
            ..
        } => {
            // Drop echoes of our own outbound user frames. The CLI
            // replays them when `replay-user-messages` is enabled; we
            // identify them by the UUID we stamped on the way out.
            if let Some(uuid_str) = &message_uuid
                && let Ok(uuid) = uuid::Uuid::parse_str(uuid_str)
                && let Some(prompt_state) = &state.prompt_state
                && let Ok(mut ps) = prompt_state.lock()
                && ps.own_uuids.remove(&uuid)
            {
                return Vec::new();
            }
            extract_tool_results(message, state)
        },

        SdkMessage::StreamEvent { .. } => unreachable!("handled above"),

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

        // Control responses are routed by the dispatcher's correlator
        // to the caller that initiated the request. If one reaches the
        // host adapter directly it means no dispatcher is installed;
        // surface it as Unknown so callers can at least see the frame.
        SdkMessage::ControlResponse { response } => vec![AgentMessage::Unknown {
            raw: serde_json::json!({
                "type": "control_response",
                "response": response,
            })
            .to_string(),
        }],

        SdkMessage::Result {
            total_cost_usd,
            duration_ms,
            num_turns,
            is_error,
            result,
            usage,
            ..
        } => {
            let mut out = Vec::new();
            // Fold result-level usage into the accumulator and emit a
            // Usage snapshot before the terminating Result.
            if let Some(u) = usage {
                let last = usage_to_host(&u);
                state.accumulated_usage.input_tokens += last.input_tokens;
                state.accumulated_usage.output_tokens += last.output_tokens;
                state.accumulated_usage.cache_creation_input_tokens +=
                    last.cache_creation_input_tokens;
                state.accumulated_usage.cache_read_input_tokens += last.cache_read_input_tokens;
                out.push(AgentMessage::Usage {
                    accumulated: state.accumulated_usage.clone(),
                    last,
                });
            }
            if is_error {
                out.push(AgentMessage::Error {
                    message: result.unwrap_or_else(|| "claude run failed".into()),
                });
            } else {
                out.push(AgentMessage::Result {
                    cost_usd: total_cost_usd,
                    duration_ms,
                    num_turns,
                });
            }
            out
        },
    }
}

fn stream_event_to_messages(msg: SdkMessage, state: &mut AdapterState) -> Vec<AgentMessage> {
    // content_block_start: register the block so subsequent deltas can
    // correlate back (tool_use id) or trigger dedup (text/thinking).
    if let Some((index, block)) = msg.as_content_block_start() {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("tool_use") => {
                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                    state.streaming_tool_ids.insert(index, id.to_string());
                }
            },
            Some("text") | Some("thinking") => {
                state.streaming_text_buffers.entry(index).or_default();
            },
            _ => {},
        }
        return Vec::new();
    }

    // content_block_stop / message_*: structural events with no host output.
    if let Some(_index) = msg.as_content_block_stop() {
        return Vec::new();
    }
    if msg.as_message_start().is_some() || msg.as_message_delta().is_some() {
        return Vec::new();
    }
    if msg.is_message_stop() {
        state.reset_streaming_state();
        return Vec::new();
    }

    // Deltas: emit a cumulative PartialText (the full block-so-far, not
    // just the new chunk) so consumers can overwrite their live view on
    // each event. A consumer that concatenated raw chunks itself would
    // leave a permanent gap on any missed event; with cumulative text,
    // the next delta corrects any prior loss.
    if let Some(text) = msg.as_text_delta() {
        let cumulative = if let Some(index) = msg.content_block_delta_index() {
            state.streamed_text_indexes.insert(index);
            let buf = state.streaming_text_buffers.entry(index).or_default();
            buf.push_str(text);
            buf.clone()
        } else {
            text.to_string()
        };
        return vec![AgentMessage::PartialText { text: cumulative }];
    }
    if let Some(text) = msg.as_thinking_delta() {
        let cumulative = if let Some(index) = msg.content_block_delta_index() {
            state.streamed_thinking_indexes.insert(index);
            let buf = state.streaming_text_buffers.entry(index).or_default();
            buf.push_str(text);
            buf.clone()
        } else {
            text.to_string()
        };
        return vec![AgentMessage::PartialText { text: cumulative }];
    }
    if let Some(delta) = msg.as_input_json_delta() {
        let id = msg
            .content_block_delta_index()
            .and_then(|i| state.streaming_tool_ids.get(&i).cloned())
            .unwrap_or_default();
        return vec![AgentMessage::PartialToolInput {
            id,
            json_delta: delta.to_string(),
        }];
    }

    let SdkMessage::StreamEvent { event, .. } = msg else {
        unreachable!();
    };
    vec![AgentMessage::Unknown {
        raw: event.to_string(),
    }]
}

fn assistant_to_messages(message: AssistantMessage, state: &mut AdapterState) -> Vec<AgentMessage> {
    let mut out = Vec::new();

    for (index, content) in message.content.into_iter().enumerate() {
        let idx = index as u64;
        match content {
            MessageContent::Text { text } => {
                // Final block is authoritative. Streaming deltas update the
                // live view; on the finalised assistant message, forward the
                // full `Text` so consumers can clear their streaming buffer
                // and record a permanent copy.
                state.streamed_text_indexes.remove(&idx);
                state.streaming_text_buffers.remove(&idx);
                out.push(AgentMessage::Text { text });
            },
            MessageContent::ToolUse { id, name, input } => {
                let tool_use = crate::messages::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                let info = tool_info_from_tool_use(
                    &tool_use,
                    state.supports_terminal,
                    state.cwd.as_deref(),
                );
                state.tool_use_cache.insert(
                    id.clone(),
                    ToolUseSnapshot {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                );

                // Special-case TodoWrite: surface it as a Plan update
                // in addition to the regular ToolUse so downstream
                // consumers can render the plan widget.
                if name == "TodoWrite" {
                    let entries = crate::tools::plan_entries(
                        &serde_json::to_value(&input).unwrap_or(serde_json::Value::Null),
                    );
                    if !entries.is_empty() {
                        out.push(AgentMessage::Plan { entries });
                    }
                }
                out.push(AgentMessage::ToolUse {
                    id,
                    name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                    kind: info.kind,
                    title: info.title,
                    content: info.content,
                    locations: info.locations,
                });
            },
            MessageContent::Thinking {
                thinking,
                signature,
            } => {
                state.streamed_thinking_indexes.remove(&idx);
                state.streaming_text_buffers.remove(&idx);
                out.push(AgentMessage::Thinking {
                    text: thinking,
                    signature,
                });
            },
            MessageContent::RedactedThinking { data } => out.push(AgentMessage::Unknown {
                raw: serde_json::json!({
                    "type": "redacted_thinking",
                    "data": data,
                })
                .to_string(),
            }),
            MessageContent::ServerToolUse { id, name, input } => {
                out.push(AgentMessage::ServerToolUse {
                    id,
                    name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                })
            },
            MessageContent::ServerToolResult {
                tool_use_id,
                content,
            } => out.push(AgentMessage::ServerToolResult {
                id: tool_use_id,
                content: match content {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                },
            }),
            MessageContent::Image { source } => out.push(AgentMessage::Unknown {
                raw: serde_json::json!({
                    "type": "image",
                    "source": source,
                })
                .to_string(),
            }),
            MessageContent::Unknown(value) => out.push(AgentMessage::Unknown {
                raw: value.to_string(),
            }),
        }
    }

    // Accumulate per-turn usage if the API reported it.
    if let Some(u) = message.usage {
        let last = usage_to_host(&u);
        state.accumulated_usage.input_tokens += last.input_tokens;
        state.accumulated_usage.output_tokens += last.output_tokens;
        state.accumulated_usage.cache_creation_input_tokens += last.cache_creation_input_tokens;
        state.accumulated_usage.cache_read_input_tokens += last.cache_read_input_tokens;
        out.push(AgentMessage::Usage {
            accumulated: state.accumulated_usage.clone(),
            last,
        });
    }

    out
}

fn usage_to_host(u: &crate::messages::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens.unwrap_or(0),
        cache_read_input_tokens: u.cache_read_input_tokens.unwrap_or(0),
    }
}

/// Extract `AgentMessage::ToolResult` entries from a wire user message.
/// Consults `state.tool_use_cache` so results can be correlated with
/// their originating tool's kind and any Bash-specific terminal meta.
fn extract_tool_results(message: UserMessage, state: &mut AdapterState) -> Vec<AgentMessage> {
    match message.content {
        UserContent::Text(_) => Vec::new(),
        UserContent::Blocks(blocks) => blocks
            .into_iter()
            .filter_map(|block| match block {
                UserContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = content.as_text();
                    let update = tool_update_from_tool_result(
                        &tool_use_id,
                        &text,
                        is_error.unwrap_or(false),
                        &state.tool_use_cache,
                        state.supports_terminal,
                    );
                    let kind = state
                        .tool_use_cache
                        .get(&tool_use_id)
                        .map(|snap| {
                            let as_tool = crate::messages::ToolUse {
                                id: snap.id.clone(),
                                name: snap.name.clone(),
                                input: snap.input.clone(),
                            };
                            tool_info_from_tool_use(
                                &as_tool,
                                state.supports_terminal,
                                state.cwd.as_deref(),
                            )
                            .kind
                        })
                        .unwrap_or(ToolKind::Other);
                    Some(AgentMessage::ToolResult {
                        id: tool_use_id,
                        content: text,
                        status: update.status.unwrap_or(ToolCallStatus::Completed),
                        kind,
                        terminal_meta: update.terminal_meta,
                    })
                },
                UserContentBlock::Text { .. } => None,
            })
            .collect(),
    }
}

#[async_trait]
impl ClaudeCodeSession for ClaudeCode {
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
            let expanded = {
                let mut state = self
                    .adapter_state
                    .lock()
                    .expect("ClaudeCode adapter_state mutex poisoned");
                sdk_message_to_agent_messages(sdk_msg, &mut state)
            };
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

    async fn interrupt(&self) -> io::Result<()> {
        ClaudeCode::interrupt(self).await.map_err(io::Error::other)
    }

    async fn set_model(&self, model_id: &str) -> io::Result<()> {
        ClaudeCode::set_model(self, model_id)
            .await
            .map_err(io::Error::other)
    }

    async fn set_permission_mode(&self, mode: &str) -> io::Result<()> {
        ClaudeCode::set_permission_mode(self, mode)
            .await
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{
        ApiKeySource, AssistantMessage, PermissionMode, ResultSubtype, Role, SystemSubtype,
    };
    use std::collections::HashMap;

    /// Convenience wrapper so existing tests don't have to thread a
    /// mutable `AdapterState` through every call. Tests that care about
    /// state accumulation (usage, tool_use_cache) use the full-argument
    /// form directly.
    fn sdk_message_to_agent_messages(msg: SdkMessage) -> Vec<AgentMessage> {
        let mut state = AdapterState::default();
        super::sdk_message_to_agent_messages(msg, &mut state)
    }

    fn assistant(content: Vec<MessageContent>) -> SdkMessage {
        SdkMessage::Assistant {
            message: AssistantMessage {
                role: Role::Assistant,
                content,
                model: None,
                usage: None,
                id: None,
                stop_reason: None,
                stop_sequence: None,
            },
            session_id: "sess".into(),
            parent_tool_use_id: None,
        }
    }

    fn system_init() -> SdkMessage {
        SdkMessage::System {
            subtype: SystemSubtype::Init,
            api_key_source: Some(ApiKeySource::None),
            cwd: Some("/tmp".into()),
            session_id: "sess-42".into(),
            tools: Some(vec!["Read".into(), "Write".into()]),
            mcp_servers: Some(vec![]),
            model: Some("claude-opus".into()),
            permission_mode: Some(PermissionMode::Default),
            state: None,
            status: None,
            text: None,
            trigger: None,
            pre_tokens: None,
            post_tokens: None,
            task_id: None,
            title: None,
            parent_tool_use_id: None,
            hook_event_name: None,
            paths: None,
            attempt: None,
            reason: None,
            extra: serde_json::Map::new(),
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
            AgentMessage::ToolUse {
                id, name, input, ..
            } => {
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
            message_uuid: None,
            parent_tool_use_id: None,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::ToolResult { id, content, .. } => {
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
            message_uuid: None,
            parent_tool_use_id: None,
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
            usage: None,
            model_usage: None,
            stop_reason: None,
            parent_tool_use_id: None,
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
            usage: None,
            model_usage: None,
            stop_reason: None,
            parent_tool_use_id: None,
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
    fn sdk_stream_event_message_start_is_silent() {
        // message_start is a structural turn-boundary marker; the
        // adapter resets streaming state on message_stop and emits
        // nothing for start/delta/stop.
        let msg = SdkMessage::StreamEvent {
            event: serde_json::json!({
                "type": "message_start",
                "message": {"id": "m1"}
            }),
            session_id: "sess-s".into(),
            parent_tool_use_id: None,
        };
        let out = sdk_message_to_agent_messages(msg);
        assert!(
            out.is_empty(),
            "structural stream events should produce no AgentMessages, got {out:?}"
        );
        // Non-stream-event fall-through still emits Unknown.
        let unknown_event = SdkMessage::StreamEvent {
            event: serde_json::json!({"type": "ping"}),
            session_id: "sess-s".into(),
            parent_tool_use_id: None,
        };
        let out = sdk_message_to_agent_messages(unknown_event);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentMessage::Unknown { raw } => {
                let reparsed: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(reparsed["type"], "ping");
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // ---- Stream-dedup + tool-id correlation ----

    fn stream_event(event: serde_json::Value) -> SdkMessage {
        SdkMessage::StreamEvent {
            event,
            session_id: "sess".into(),
            parent_tool_use_id: None,
        }
    }

    #[test]
    fn streamed_text_still_emits_final_text_for_consumer_flush() {
        let mut state = AdapterState::default();
        // content_block_start announces a text block at index 0.
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            })),
            &mut state,
        );
        // Text delta for the same index.
        let out = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello "}
            })),
            &mut state,
        );
        assert!(matches!(out.as_slice(), [AgentMessage::PartialText { text }] if text == "hello "));
        // The full Assistant message arrives with the authoritative Text.
        // The adapter forwards it so consumers can clear their streaming
        // buffer and record the permanent copy.
        let out = super::sdk_message_to_agent_messages(
            assistant(vec![MessageContent::Text {
                text: "hello world".into(),
            }]),
            &mut state,
        );
        assert!(
            matches!(out.as_slice(), [AgentMessage::Text { text }] if text == "hello world"),
            "expected final Text to flow through, got {out:?}"
        );
        assert!(
            !state.streamed_text_indexes.contains(&0),
            "streamed index should clear once final Text is emitted"
        );
    }

    #[test]
    fn streamed_thinking_still_emits_final_thinking_for_consumer_flush() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            })),
            &mut state,
        );
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "hmm"}
            })),
            &mut state,
        );
        let out = super::sdk_message_to_agent_messages(
            assistant(vec![MessageContent::Thinking {
                thinking: "hmm".into(),
                signature: "s".into(),
            }]),
            &mut state,
        );
        assert!(
            matches!(out.as_slice(), [AgentMessage::Thinking { text, .. }] if text == "hmm"),
            "expected final Thinking to flow through, got {out:?}"
        );
    }

    #[test]
    fn text_deltas_emit_cumulative_partial_text() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            })),
            &mut state,
        );
        let deltas = ["Hello", ", ", "world", "!"];
        let want = ["Hello", "Hello, ", "Hello, world", "Hello, world!"];
        for (delta, expected) in deltas.iter().zip(want.iter()) {
            let out = super::sdk_message_to_agent_messages(
                stream_event(serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": delta}
                })),
                &mut state,
            );
            assert!(
                matches!(out.as_slice(), [AgentMessage::PartialText { text }] if text == expected),
                "delta {delta:?}: expected cumulative {expected:?}, got {out:?}"
            );
        }
    }

    #[test]
    fn thinking_deltas_emit_cumulative_partial_text() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            })),
            &mut state,
        );
        for (delta, expected) in [("hmm ", "hmm "), ("let me think", "hmm let me think")] {
            let out = super::sdk_message_to_agent_messages(
                stream_event(serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": delta}
                })),
                &mut state,
            );
            assert!(
                matches!(out.as_slice(), [AgentMessage::PartialText { text }] if text == expected),
                "delta {delta:?}: expected cumulative {expected:?}, got {out:?}"
            );
        }
    }

    #[test]
    fn input_json_delta_correlates_to_tool_id_from_block_start() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_xyz",
                    "name": "Bash",
                    "input": {}
                }
            })),
            &mut state,
        );
        let out = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}
            })),
            &mut state,
        );
        match out.as_slice() {
            [AgentMessage::PartialToolInput { id, json_delta }] => {
                assert_eq!(id, "toolu_xyz");
                assert_eq!(json_delta, "{\"cmd\":");
            },
            other => panic!("expected PartialToolInput, got {other:?}"),
        }
    }

    #[test]
    fn input_json_delta_without_prior_block_start_leaves_id_empty() {
        let mut state = AdapterState::default();
        let out = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{"}
            })),
            &mut state,
        );
        match out.as_slice() {
            [AgentMessage::PartialToolInput { id, .. }] => assert!(id.is_empty()),
            other => panic!("expected PartialToolInput, got {other:?}"),
        }
    }

    #[test]
    fn message_stop_resets_streaming_state() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            })),
            &mut state,
        );
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hi"}
            })),
            &mut state,
        );
        assert!(state.streamed_text_indexes.contains(&0));
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({"type": "message_stop"})),
            &mut state,
        );
        assert!(state.streamed_text_indexes.is_empty());
        assert!(state.streaming_tool_ids.is_empty());
        assert!(state.streaming_text_buffers.is_empty());
    }

    #[test]
    fn content_block_stop_is_silent_but_leaves_streamed_set_intact() {
        let mut state = AdapterState::default();
        let _ = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "x"}
            })),
            &mut state,
        );
        let out = super::sdk_message_to_agent_messages(
            stream_event(serde_json::json!({
                "type": "content_block_stop",
                "index": 0
            })),
            &mut state,
        );
        assert!(out.is_empty());
        assert!(state.streamed_text_indexes.contains(&0));
    }

    #[test]
    fn user_echo_with_known_uuid_is_dropped() {
        use std::sync::{Arc, Mutex};
        let prompt_state = Arc::new(Mutex::new(crate::claude_code::PromptState::default()));
        let uuid = uuid::Uuid::new_v4();
        prompt_state.lock().unwrap().own_uuids.insert(uuid);
        let mut state = AdapterState {
            prompt_state: Some(prompt_state.clone()),
            ..Default::default()
        };
        let msg = SdkMessage::User {
            message: UserMessage::from_text("our own prompt"),
            session_id: "sess".into(),
            message_uuid: Some(uuid.to_string()),
            parent_tool_use_id: None,
        };
        let out = super::sdk_message_to_agent_messages(msg, &mut state);
        assert!(out.is_empty(), "echo must drop, got {out:?}");
        // Uuid must be consumed so a subsequent replay wouldn't drop a
        // legitimate user message.
        assert!(prompt_state.lock().unwrap().own_uuids.is_empty());
    }

    #[test]
    fn user_echo_with_unknown_uuid_is_forwarded() {
        use std::sync::{Arc, Mutex};
        let prompt_state = Arc::new(Mutex::new(crate::claude_code::PromptState::default()));
        let mut state = AdapterState {
            prompt_state: Some(prompt_state),
            ..Default::default()
        };
        let msg = SdkMessage::User {
            message: UserMessage::from_tool_result("tool_a", "data"),
            session_id: "sess".into(),
            message_uuid: Some(uuid::Uuid::new_v4().to_string()),
            parent_tool_use_id: None,
        };
        let out = super::sdk_message_to_agent_messages(msg, &mut state);
        assert!(
            out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult { id, .. } if id == "tool_a")),
            "unknown UUID must not drop tool result, got {out:?}"
        );
    }

    #[test]
    fn user_without_uuid_skips_echo_check() {
        // A User frame without a UUID (e.g. tool result the CLI
        // injected itself) should route normally, not be dropped.
        let mut state = AdapterState::default();
        let msg = SdkMessage::User {
            message: UserMessage::from_tool_result("tool_x", "stdout"),
            session_id: "sess".into(),
            message_uuid: None,
            parent_tool_use_id: None,
        };
        let out = super::sdk_message_to_agent_messages(msg, &mut state);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], AgentMessage::ToolResult { .. }));
    }

    // ---- System-subtype coverage ----

    fn system_frame(subtype: SystemSubtype, extras: serde_json::Value) -> SdkMessage {
        let mut val = serde_json::json!({
            "type": "system",
            "session_id": "sess-sub",
        });
        if let serde_json::Value::Object(map) = &extras
            && let serde_json::Value::Object(base) = &mut val
        {
            for (k, v) in map {
                base.insert(k.clone(), v.clone());
            }
        }
        match &subtype {
            SystemSubtype::Init => val["subtype"] = "init".into(),
            SystemSubtype::Status => val["subtype"] = "status".into(),
            SystemSubtype::CompactBoundary => val["subtype"] = "compact_boundary".into(),
            SystemSubtype::LocalCommandOutput => val["subtype"] = "local_command_output".into(),
            SystemSubtype::SessionStateChanged => val["subtype"] = "session_state_changed".into(),
            SystemSubtype::HookStarted => val["subtype"] = "hook_started".into(),
            SystemSubtype::HookProgress => val["subtype"] = "hook_progress".into(),
            SystemSubtype::HookResponse => val["subtype"] = "hook_response".into(),
            SystemSubtype::FilesPersisted => val["subtype"] = "files_persisted".into(),
            SystemSubtype::TaskStarted => val["subtype"] = "task_started".into(),
            SystemSubtype::TaskNotification => val["subtype"] = "task_notification".into(),
            SystemSubtype::TaskProgress => val["subtype"] = "task_progress".into(),
            SystemSubtype::TaskUpdated => val["subtype"] = "task_updated".into(),
            SystemSubtype::ElicitationComplete => val["subtype"] = "elicitation_complete".into(),
            SystemSubtype::ApiRetry => val["subtype"] = "api_retry".into(),
            SystemSubtype::Unknown(s) => val["subtype"] = s.clone().into(),
        }
        serde_json::from_value(val).unwrap()
    }

    #[test]
    fn session_state_changed_maps_to_state_changed_event() {
        let msg = system_frame(
            SystemSubtype::SessionStateChanged,
            serde_json::json!({"state": "idle"}),
        );
        let out = sdk_message_to_agent_messages(msg);
        match &out[0] {
            AgentMessage::SessionState(SessionStateEvent::StateChanged { state }) => {
                assert_eq!(state, "idle")
            },
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn compact_boundary_maps_to_compact_boundary_event() {
        let msg = system_frame(
            SystemSubtype::CompactBoundary,
            serde_json::json!({
                "trigger": "context_limit_pressure",
                "pre_tokens": 100,
                "post_tokens": 20
            }),
        );
        let out = sdk_message_to_agent_messages(msg);
        match &out[0] {
            AgentMessage::SessionState(SessionStateEvent::CompactBoundary {
                trigger,
                pre_tokens,
                post_tokens,
            }) => {
                assert_eq!(trigger.as_deref(), Some("context_limit_pressure"));
                assert_eq!(*pre_tokens, Some(100));
                assert_eq!(*post_tokens, Some(20));
            },
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn api_retry_maps_to_api_retry_event() {
        let msg = system_frame(
            SystemSubtype::ApiRetry,
            serde_json::json!({"attempt": 3, "reason": "overloaded"}),
        );
        let out = sdk_message_to_agent_messages(msg);
        assert!(matches!(
            &out[0],
            AgentMessage::SessionState(SessionStateEvent::ApiRetry {
                attempt: Some(3),
                reason: Some(s),
            }) if s == "overloaded"
        ));
    }

    #[test]
    fn files_persisted_maps_to_files_persisted() {
        let msg = system_frame(
            SystemSubtype::FilesPersisted,
            serde_json::json!({"paths": ["/tmp/a", "/tmp/b"]}),
        );
        let out = sdk_message_to_agent_messages(msg);
        match &out[0] {
            AgentMessage::FilesPersisted { paths } => {
                assert_eq!(paths.len(), 2);
                assert_eq!(paths[0].to_string_lossy(), "/tmp/a");
            },
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn task_events_round_trip() {
        let started = system_frame(
            SystemSubtype::TaskStarted,
            serde_json::json!({"task_id": "t1", "title": "run"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(started)[0],
            AgentMessage::TaskEvent(TaskEvent::Started { .. })
        ));
        let notif = system_frame(
            SystemSubtype::TaskNotification,
            serde_json::json!({"task_id": "t1", "text": "progress"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(notif)[0],
            AgentMessage::TaskEvent(TaskEvent::Notification { .. })
        ));
        let prog = system_frame(
            SystemSubtype::TaskProgress,
            serde_json::json!({"task_id": "t1"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(prog)[0],
            AgentMessage::TaskEvent(TaskEvent::Progress { .. })
        ));
        let upd = system_frame(
            SystemSubtype::TaskUpdated,
            serde_json::json!({"task_id": "t1"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(upd)[0],
            AgentMessage::TaskEvent(TaskEvent::Updated { .. })
        ));
    }

    #[test]
    fn hook_events_round_trip() {
        let started = system_frame(
            SystemSubtype::HookStarted,
            serde_json::json!({"hook_event_name": "PreToolUse"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(started)[0],
            AgentMessage::Hook(HookLifecycleEvent::Started { .. })
        ));
        let progress = system_frame(
            SystemSubtype::HookProgress,
            serde_json::json!({"hook_event_name": "PreToolUse"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(progress)[0],
            AgentMessage::Hook(HookLifecycleEvent::Progress { .. })
        ));
        let resp = system_frame(
            SystemSubtype::HookResponse,
            serde_json::json!({"hook_event_name": "PreToolUse"}),
        );
        assert!(matches!(
            sdk_message_to_agent_messages(resp)[0],
            AgentMessage::Hook(HookLifecycleEvent::Response { .. })
        ));
    }

    #[test]
    fn local_command_output_maps_to_event() {
        let msg = system_frame(
            SystemSubtype::LocalCommandOutput,
            serde_json::json!({"text": "hello"}),
        );
        let out = sdk_message_to_agent_messages(msg);
        match &out[0] {
            AgentMessage::SessionState(SessionStateEvent::LocalCommandOutput { text }) => {
                assert_eq!(text, "hello")
            },
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_system_subtype_preserved_as_unknown() {
        let msg = system_frame(
            SystemSubtype::Unknown("brand_new".into()),
            serde_json::json!({"extra_field": 42}),
        );
        let out = sdk_message_to_agent_messages(msg);
        match &out[0] {
            AgentMessage::Unknown { raw } => {
                assert!(raw.contains("brand_new"));
            },
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn non_streamed_text_in_full_message_emits_normally() {
        // Dedup only suppresses indexes that were streamed. An
        // Assistant message whose Text blocks never streamed should
        // still produce Text AgentMessages.
        let mut state = AdapterState::default();
        let out = super::sdk_message_to_agent_messages(
            assistant(vec![MessageContent::Text {
                text: "not streamed".into(),
            }]),
            &mut state,
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, AgentMessage::Text { text } if text == "not streamed")),
            "non-streamed Text must still emit, got {out:?}"
        );
    }
}
