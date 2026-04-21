use crate::messages::*;
use std::collections::HashMap;

#[test]
fn test_message_type_detection() {
    let result = SdkMessage::Result {
        subtype: ResultSubtype::Success,
        duration_ms: 1000,
        duration_api_ms: 800,
        is_error: false,
        num_turns: 1,
        result: Some("Done".to_string()),
        session_id: "test".to_string(),
        total_cost_usd: 0.001,
        usage: None,
        model_usage: None,
        stop_reason: None,
        parent_tool_use_id: None,
    };

    assert!(result.is_terminal());
    assert_eq!(result.session_id(), "test");
    assert_eq!(result.message_type(), "result");
}

#[test]
fn test_assistant_message_helpers() {
    let msg = AssistantMessage {
        role: Role::Assistant,
        content: vec![
            MessageContent::Text {
                text: "Let me help.".to_string(),
            },
            MessageContent::ToolUse {
                id: "tool_123".to_string(),
                name: "Read".to_string(),
                input: HashMap::new(),
            },
            MessageContent::Text {
                text: "Done!".to_string(),
            },
        ],
        model: None,
        usage: None,
        id: None,
        stop_reason: None,
        stop_sequence: None,
    };

    assert_eq!(msg.get_text_content(), "Let me help.\nDone!");
    assert_eq!(msg.get_tool_uses().len(), 1);
    assert!(!msg.is_tool_only());
    assert!(msg.has_tool_uses());
}

#[test]
fn test_user_message_constructors() {
    let text_msg = UserMessage::from_text("Hello");
    assert_eq!(text_msg.as_text(), Some("Hello"));
    assert!(!text_msg.is_tool_result());

    let tool_msg = UserMessage::from_tool_result("tool_123", "Success");
    assert!(tool_msg.is_tool_result());
    assert_eq!(tool_msg.as_text(), None);
}

#[test]
fn message_content_known_text_parses() {
    let json = r#"{"type":"text","text":"hi"}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::Text { text } => assert_eq!(text, "hi"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn message_content_known_tool_use_parses() {
    let json = r#"{"type":"tool_use","id":"abc","name":"Read","input":{"path":"/tmp"}}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::ToolUse { id, name, input } => {
            assert_eq!(id, "abc");
            assert_eq!(name, "Read");
            assert_eq!(
                input.get("path"),
                Some(&serde_json::Value::String("/tmp".to_string()))
            );
        },
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn message_content_image_parses() {
    let json = r#"{"type":"image","source":{"kind":"base64","data":"xyz"}}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::Image { source } => {
            assert_eq!(source.get("data").and_then(|d| d.as_str()), Some("xyz"));
            assert_eq!(source.get("kind").and_then(|k| k.as_str()), Some("base64"));
        },
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn message_content_unknown_tag_preserved() {
    let json = r#"{"type":"audio","source":{"data":"xyz"}}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::Unknown(value) => {
            assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("audio"));
            assert_eq!(
                value
                    .get("source")
                    .and_then(|s| s.get("data"))
                    .and_then(|d| d.as_str()),
                Some("xyz")
            );
        },
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn message_content_malformed_tool_use_falls_back() {
    let json = r#"{"type":"tool_use","id":"x"}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    assert!(
        parsed.is_unknown(),
        "expected Unknown for malformed tool_use, got {parsed:?}"
    );
}

#[test]
fn sdk_assistant_with_unknown_content_still_parses() {
    let json = r#"{
        "type": "assistant",
        "session_id": "sess-1",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "server_tool_use", "id": "srv_1", "tool": "web_search"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": "/x"}}
            ]
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    let SdkMessage::Assistant { message, .. } = parsed else {
        panic!("expected Assistant variant");
    };
    assert_eq!(message.content.len(), 3);
    assert!(message.content[0].is_text());
    assert!(message.content[1].is_unknown());
    assert!(message.content[2].is_tool_use());
}

#[test]
fn message_content_thinking_parses() {
    let json = r#"{"type":"thinking","thinking":"let me think","signature":"sig-abc"}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "let me think");
            assert_eq!(signature, "sig-abc");
        },
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn message_content_redacted_thinking_parses() {
    let json = r#"{"type":"redacted_thinking","data":"encrypted-blob"}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::RedactedThinking { data } => assert_eq!(data, "encrypted-blob"),
        other => panic!("expected RedactedThinking, got {other:?}"),
    }
}

#[test]
fn message_content_server_tool_use_parses() {
    let json =
        r#"{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"rust"}}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::ServerToolUse { id, name, input } => {
            assert_eq!(id, "srv_1");
            assert_eq!(name, "web_search");
            assert_eq!(input.get("query"), Some(&serde_json::json!("rust")));
        },
        other => panic!("expected ServerToolUse, got {other:?}"),
    }
}

#[test]
fn message_content_server_tool_result_parses() {
    let json = r#"{"type":"server_tool_result","tool_use_id":"srv_1","content":"found"}"#;
    let parsed: MessageContent = serde_json::from_str(json).unwrap();
    match parsed {
        MessageContent::ServerToolResult {
            tool_use_id,
            content,
        } => {
            assert_eq!(tool_use_id, "srv_1");
            assert_eq!(content, serde_json::json!("found"));
        },
        other => panic!("expected ServerToolResult, got {other:?}"),
    }
}

#[test]
fn user_content_block_tool_result_accepts_plain_string() {
    let json = r#"{"type":"tool_result","tool_use_id":"t1","content":"ok"}"#;
    let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
    match parsed {
        UserContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "t1");
            assert_eq!(content.as_text(), "ok");
            assert_eq!(is_error, None);
        },
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn user_content_block_tool_result_accepts_structured_blocks() {
    let json = r#"{
        "type":"tool_result",
        "tool_use_id":"t1",
        "content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],
        "is_error": true
    }"#;
    let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
    match parsed {
        UserContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "t1");
            assert_eq!(is_error, Some(true));
            let flattened = content.as_text();
            assert!(flattened.contains("line1"));
            assert!(flattened.contains("line2"));
        },
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn user_content_block_tool_result_is_error_absent_defaults_none() {
    let json = r#"{"type":"tool_result","tool_use_id":"t1","content":"ok"}"#;
    let parsed: UserContentBlock = serde_json::from_str(json).unwrap();
    match parsed {
        UserContentBlock::ToolResult { is_error, .. } => assert_eq!(is_error, None),
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn stream_event_parses_and_exposes_text_delta() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello "}
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.message_type(), "stream_event");
    assert_eq!(parsed.session_id(), "sess-s");
    assert_eq!(parsed.as_text_delta(), Some("hello "));
}

#[test]
fn stream_event_non_text_delta_returns_none() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"x\":"}
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.as_text_delta(), None);
}

#[test]
fn stream_event_message_start_returns_none() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {"type": "message_start", "message": {}}
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.as_text_delta(), None);
}

#[test]
fn non_stream_event_text_delta_returns_none() {
    let msg = SdkMessage::Result {
        subtype: ResultSubtype::Success,
        duration_ms: 1,
        duration_api_ms: 1,
        is_error: false,
        num_turns: 1,
        result: None,
        session_id: "sess".into(),
        total_cost_usd: 0.0,
        usage: None,
        model_usage: None,
        stop_reason: None,
        parent_tool_use_id: None,
    };
    assert_eq!(msg.as_text_delta(), None);
}

#[test]
fn control_request_can_use_tool_parses_and_extracts() {
    let json = r#"{
        "type": "control_request",
        "request_id": "req_7",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {"command": "ls /"},
            "tool_use_id": "toolu_1"
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.message_type(), "control_request");
    let req = parsed
        .as_can_use_tool()
        .expect("expected CanUseToolRequest view");
    assert_eq!(req.request_id, "req_7");
    assert_eq!(req.tool_name, "Bash");
    assert_eq!(req.tool_use_id, Some("toolu_1"));
    assert_eq!(req.input["command"], serde_json::json!("ls /"));
    assert!(parsed.as_hook_callback().is_none());
}

#[test]
fn control_request_hook_callback_parses_and_extracts() {
    let json = r#"{
        "type": "control_request",
        "request_id": "req_8",
        "request": {
            "subtype": "hook_callback",
            "callback_id": "cb_0",
            "input": {"foo": "bar"},
            "tool_use_id": "toolu_2"
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    let req = parsed
        .as_hook_callback()
        .expect("expected HookCallbackRequest view");
    assert_eq!(req.request_id, "req_8");
    assert_eq!(req.callback_id, "cb_0");
    assert_eq!(req.tool_use_id, Some("toolu_2"));
    assert!(parsed.as_can_use_tool().is_none());
}

#[test]
fn control_response_success_serializes_to_expected_shape() {
    let resp = ControlResponse::success(
        "req_7",
        serde_json::json!({"behavior": "allow", "updatedInput": {"command": "ls /"}}),
    );
    let encoded = serde_json::to_value(&resp).unwrap();
    assert_eq!(encoded["type"], "control_response");
    assert_eq!(encoded["response"]["subtype"], "success");
    assert_eq!(encoded["response"]["request_id"], "req_7");
    assert_eq!(encoded["response"]["response"]["behavior"], "allow");
    assert_eq!(
        encoded["response"]["response"]["updatedInput"]["command"],
        "ls /"
    );
}

#[test]
fn control_response_error_serializes_to_expected_shape() {
    let resp = ControlResponse::error("req_7", "no callback");
    let encoded = serde_json::to_value(&resp).unwrap();
    assert_eq!(encoded["type"], "control_response");
    assert_eq!(encoded["response"]["subtype"], "error");
    assert_eq!(encoded["response"]["request_id"], "req_7");
    assert_eq!(encoded["response"]["error"], "no callback");
}

#[test]
fn system_subtype_falls_back_to_unknown_variant() {
    let parsed: SystemSubtype = serde_json::from_str("\"frobnication\"").unwrap();
    assert_eq!(parsed, SystemSubtype::Unknown("frobnication".into()));
    // Known values still map correctly.
    let init: SystemSubtype = serde_json::from_str("\"init\"").unwrap();
    assert_eq!(init, SystemSubtype::Init);
    let compact: SystemSubtype = serde_json::from_str("\"compact_boundary\"").unwrap();
    assert_eq!(compact, SystemSubtype::CompactBoundary);
}

#[test]
fn result_subtype_falls_back_to_unknown_variant() {
    let parsed: ResultSubtype = serde_json::from_str("\"error_something_new\"").unwrap();
    assert_eq!(parsed, ResultSubtype::Unknown("error_something_new".into()));
    let max_budget: ResultSubtype = serde_json::from_str("\"error_max_budget_usd\"").unwrap();
    assert_eq!(max_budget, ResultSubtype::ErrorMaxBudgetUsd);
}

#[test]
fn system_non_init_subtype_parses_with_missing_init_fields() {
    // session_state_changed has no cwd/tools/model/permission_mode,
    // only session_id and a `state` payload.
    let json = r#"{
        "type": "system",
        "subtype": "session_state_changed",
        "session_id": "sess-9",
        "state": "idle"
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    match parsed {
        SdkMessage::System {
            subtype,
            session_id,
            cwd,
            tools,
            model,
            state,
            ..
        } => {
            assert_eq!(subtype, SystemSubtype::SessionStateChanged);
            assert_eq!(session_id, "sess-9");
            assert!(cwd.is_none());
            assert!(tools.is_none());
            assert!(model.is_none());
            assert_eq!(state.as_deref(), Some("idle"));
        },
        other => panic!("expected System, got {other:?}"),
    }
}

#[test]
fn system_unknown_subtype_parses_and_preserves_extra_fields() {
    let json = r#"{
        "type": "system",
        "subtype": "brand_new_event",
        "session_id": "sess-10",
        "payload": {"foo": 42},
        "other_field": "value"
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    match parsed {
        SdkMessage::System {
            subtype,
            session_id,
            extra,
            ..
        } => {
            assert_eq!(subtype, SystemSubtype::Unknown("brand_new_event".into()));
            assert_eq!(session_id, "sess-10");
            assert!(extra.contains_key("payload"));
            assert!(extra.contains_key("other_field"));
        },
        other => panic!("expected System, got {other:?}"),
    }
}

#[test]
fn system_compact_boundary_parses_with_token_fields() {
    let json = r#"{
        "type": "system",
        "subtype": "compact_boundary",
        "session_id": "sess-11",
        "trigger": "context_limit_pressure",
        "pre_tokens": 50000,
        "post_tokens": 10000
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    match parsed {
        SdkMessage::System {
            subtype,
            trigger,
            pre_tokens,
            post_tokens,
            ..
        } => {
            assert_eq!(subtype, SystemSubtype::CompactBoundary);
            assert_eq!(trigger.as_deref(), Some("context_limit_pressure"));
            assert_eq!(pre_tokens, Some(50000));
            assert_eq!(post_tokens, Some(10000));
        },
        other => panic!("expected System, got {other:?}"),
    }
}

#[test]
fn assistant_message_parses_with_usage_and_model() {
    let json = r#"{
        "type": "assistant",
        "session_id": "sess-12",
        "message": {
            "role": "assistant",
            "id": "msg_01",
            "model": "claude-sonnet-4-5",
            "content": [{"type":"text","text":"hello"}],
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "cache_read_input_tokens": 100
            },
            "stop_reason": "end_turn"
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    match parsed {
        SdkMessage::Assistant { message, .. } => {
            assert_eq!(message.model.as_deref(), Some("claude-sonnet-4-5"));
            assert_eq!(message.id.as_deref(), Some("msg_01"));
            assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
            let usage = message.usage.expect("usage present");
            assert_eq!(usage.input_tokens, 42);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.cache_read_input_tokens, Some(100));
        },
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn result_parses_with_usage_and_stop_reason() {
    let json = r#"{
        "type": "result",
        "subtype": "success",
        "session_id": "sess-13",
        "duration_ms": 1000,
        "duration_api_ms": 800,
        "is_error": false,
        "num_turns": 2,
        "total_cost_usd": 0.01,
        "usage": {"input_tokens": 12, "output_tokens": 3},
        "stop_reason": "end_turn"
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    match parsed {
        SdkMessage::Result {
            usage, stop_reason, ..
        } => {
            let usage = usage.expect("usage present");
            assert_eq!(usage.input_tokens, 12);
            assert_eq!(usage.output_tokens, 3);
            assert_eq!(stop_reason, Some(StopReason::EndTurn));
        },
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn stream_event_input_json_delta_accessor() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.as_input_json_delta(), Some("{\"cmd\":"));
    assert_eq!(parsed.as_text_delta(), None);
}

#[test]
fn stream_event_thinking_delta_accessor() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "hmm"}
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.as_thinking_delta(), Some("hmm"));
}

#[test]
fn stream_event_content_block_start_accessor() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {
            "type": "content_block_start",
            "index": 3,
            "content_block": {"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}
        }
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    let (index, block) = parsed.as_content_block_start().expect("should extract");
    assert_eq!(index, 3);
    assert_eq!(block.get("name").and_then(|v| v.as_str()), Some("Bash"));
}

#[test]
fn stream_event_content_block_stop_accessor() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {"type": "content_block_stop", "index": 2}
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.as_content_block_stop(), Some(2));
}

#[test]
fn stream_event_message_stop_accessor() {
    let json = r#"{
        "type": "stream_event",
        "session_id": "sess-s",
        "event": {"type": "message_stop"}
    }"#;
    let parsed: SdkMessage = serde_json::from_str(json).unwrap();
    assert!(parsed.is_message_stop());
}
