//! Control-protocol dispatcher.
//!
//! When a [`PermissionCallback`] is registered on
//! [`ClaudeCodeBuilder`](super::builder::ClaudeCodeBuilder), the builder
//! interposes [`run_dispatcher`] between the Process stdout channel and
//! the [`ClaudeCode`](super::ClaudeCode) receiver:
//!
//! ```text
//!   child stdout
//!        |
//!        v
//!   stdout handler  --inner_tx-->  inner_rx
//!                                    |
//!                                    v
//!                              run_dispatcher
//!                                    |
//!              control_request?      +-- outer_tx (host-visible)
//!                    |                           |
//!                    v                           v
//!           callback + write response    ClaudeCode::recv()
//!                    |
//!                    v
//!              stdin_tx (to CLI)
//! ```
//!
//! For messages other than `control_request`, the dispatcher simply
//! forwards from `inner_rx` to `outer_tx`. For `control_request` it
//! invokes the registered callback, serializes a [`ControlResponse`],
//! and pushes it back through the shared stdin sender so the stdin
//! handler writes it to child stdin alongside normal user messages.
//!
//! Hook callbacks are answered with a no-op `{}` success response for
//! now; the user trait for hook handling is deferred to a follow-up.

use crate::messages::{CanUseToolRequest, ControlResponse, HookCallbackRequest, SdkMessage};
use std::sync::Arc;
use stoat::host::{PermissionCallback, PermissionResult, ToolPermissionContext};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Dependencies the dispatcher holds for the lifetime of the session.
pub(crate) struct DispatcherDeps {
    pub permission_callback: Option<Arc<dyn PermissionCallback>>,
    pub stdin_tx: mpsc::Sender<String>,
}

/// Long-running task that drains `inner_rx`, intercepts control
/// requests, and forwards everything else to `outer_tx`. Exits when
/// `inner_rx` closes.
pub(crate) async fn run_dispatcher(
    mut inner_rx: mpsc::Receiver<SdkMessage>,
    outer_tx: mpsc::Sender<SdkMessage>,
    deps: DispatcherDeps,
) {
    while let Some(msg) = inner_rx.recv().await {
        match &msg {
            SdkMessage::ControlRequest { .. } => {
                handle_control_request(&msg, &deps).await;
                // Control requests are consumed here and NOT forwarded
                // to outer_tx: the host never sees raw control frames.
            },
            _ => {
                if outer_tx.send(msg).await.is_err() {
                    debug!("dispatcher: outer receiver dropped, exiting");
                    return;
                }
            },
        }
    }
}

async fn handle_control_request(msg: &SdkMessage, deps: &DispatcherDeps) {
    if let Some(req) = msg.as_can_use_tool() {
        dispatch_can_use_tool(req, deps).await;
    } else if let Some(req) = msg.as_hook_callback() {
        dispatch_hook_callback(req, deps).await;
    } else {
        let SdkMessage::ControlRequest {
            request_id,
            request,
        } = msg
        else {
            unreachable!("handle_control_request called on non-ControlRequest");
        };
        let subtype = request
            .get("subtype")
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        warn!(
            "dispatcher: unsupported control_request subtype '{}' (request_id {})",
            subtype, request_id
        );
        let response =
            ControlResponse::error(request_id, format!("unsupported subtype '{subtype}'"));
        send_response(response, deps).await;
    }
}

async fn dispatch_can_use_tool(req: CanUseToolRequest<'_>, deps: &DispatcherDeps) {
    let Some(callback) = deps.permission_callback.clone() else {
        let response = ControlResponse::error(
            req.request_id,
            "no permission callback registered on the host",
        );
        send_response(response, deps).await;
        return;
    };

    let input_json = req.input.to_string();
    let suggestions_json = req.permission_suggestions.map(|v| v.to_string());
    let context = ToolPermissionContext {
        suggestions_json: suggestions_json.as_deref(),
        tool_use_id: req.tool_use_id,
        agent_id: req.agent_id,
        blocked_path: req.blocked_path,
    };

    let request_id = req.request_id.to_string();
    let original_input = req.input.clone();

    let result = callback
        .can_use_tool(req.tool_name, &input_json, context)
        .await;

    let body = match result {
        PermissionResult::Allow {
            updated_input_json,
            updated_permissions_json,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("behavior".to_string(), "allow".into());
            let updated_input_value = match updated_input_json {
                Some(s) => serde_json::from_str::<serde_json::Value>(&s).unwrap_or(original_input),
                None => original_input,
            };
            map.insert("updatedInput".to_string(), updated_input_value);
            if let Some(permissions_str) = updated_permissions_json
                && let Ok(permissions_value) =
                    serde_json::from_str::<serde_json::Value>(&permissions_str)
            {
                map.insert("updatedPermissions".to_string(), permissions_value);
            }
            serde_json::Value::Object(map)
        },
        PermissionResult::Deny { message, interrupt } => {
            let mut map = serde_json::Map::new();
            map.insert("behavior".to_string(), "deny".into());
            map.insert("message".to_string(), message.into());
            if interrupt {
                map.insert("interrupt".to_string(), true.into());
            }
            serde_json::Value::Object(map)
        },
    };

    send_response(ControlResponse::success(request_id, body), deps).await;
}

async fn dispatch_hook_callback(req: HookCallbackRequest<'_>, deps: &DispatcherDeps) {
    debug!(
        "dispatcher: hook_callback {} (tool_use_id {:?}) -> no-op reply",
        req.callback_id, req.tool_use_id
    );
    let response = ControlResponse::success(req.request_id, serde_json::json!({}));
    send_response(response, deps).await;
}

async fn send_response(response: ControlResponse, deps: &DispatcherDeps) {
    let line = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            warn!("dispatcher: failed to serialize control_response: {e}");
            return;
        },
    };
    if deps.stdin_tx.send(line).await.is_err() {
        warn!("dispatcher: stdin channel closed, cannot deliver control_response");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Test callback that records each invocation and replies from a
    /// scripted queue of results.
    struct FakeCallback {
        seen: Mutex<Vec<(String, String)>>,
        responses: Mutex<std::collections::VecDeque<PermissionResult>>,
    }

    impl FakeCallback {
        fn with_responses(responses: Vec<PermissionResult>) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }
    }

    #[async_trait]
    impl PermissionCallback for FakeCallback {
        async fn can_use_tool(
            &self,
            tool_name: &str,
            input_json: &str,
            _ctx: ToolPermissionContext<'_>,
        ) -> PermissionResult {
            self.seen
                .lock()
                .unwrap()
                .push((tool_name.to_string(), input_json.to_string()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(PermissionResult::allow)
        }
    }

    fn control_request_can_use_tool(
        request_id: &str,
        tool_name: &str,
        command: &str,
    ) -> SdkMessage {
        SdkMessage::ControlRequest {
            request_id: request_id.to_string(),
            request: serde_json::json!({
                "subtype": "can_use_tool",
                "tool_name": tool_name,
                "input": {"command": command},
                "tool_use_id": "toolu_test",
            }),
        }
    }

    async fn run_once(
        inbound: Vec<SdkMessage>,
        callback: Option<Arc<dyn PermissionCallback>>,
    ) -> (Vec<String>, Vec<SdkMessage>) {
        let (inner_tx, inner_rx) = mpsc::channel::<SdkMessage>(8);
        let (outer_tx, mut outer_rx) = mpsc::channel::<SdkMessage>(8);
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(8);

        for msg in inbound {
            inner_tx.send(msg).await.unwrap();
        }
        drop(inner_tx);

        let deps = DispatcherDeps {
            permission_callback: callback,
            stdin_tx,
        };
        run_dispatcher(inner_rx, outer_tx, deps).await;

        let mut control_lines = Vec::new();
        while let Ok(line) = stdin_rx.try_recv() {
            control_lines.push(line);
        }
        let mut forwarded = Vec::new();
        while let Ok(msg) = outer_rx.try_recv() {
            forwarded.push(msg);
        }
        (control_lines, forwarded)
    }

    #[tokio::test]
    async fn dispatcher_allows_and_writes_response() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::allow()]);
        let (lines, forwarded) = run_once(
            vec![control_request_can_use_tool("req_1", "Bash", "ls /")],
            Some(callback.clone() as Arc<dyn PermissionCallback>),
        )
        .await;
        assert!(
            forwarded.is_empty(),
            "control requests must not be forwarded"
        );
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req_1");
        assert_eq!(parsed["response"]["response"]["behavior"], "allow");
        assert_eq!(
            parsed["response"]["response"]["updatedInput"]["command"],
            "ls /"
        );
        let seen = callback.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "Bash");
    }

    #[tokio::test]
    async fn dispatcher_denies_with_message() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::deny("nope")]);
        let (lines, _forwarded) = run_once(
            vec![control_request_can_use_tool("req_2", "Bash", "rm -rf /")],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["response"]["behavior"], "deny");
        assert_eq!(parsed["response"]["response"]["message"], "nope");
    }

    #[tokio::test]
    async fn dispatcher_errors_when_no_callback() {
        let (lines, _forwarded) = run_once(
            vec![control_request_can_use_tool("req_3", "Bash", "ls")],
            None,
        )
        .await;
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["subtype"], "error");
        assert_eq!(parsed["response"]["request_id"], "req_3");
    }

    #[tokio::test]
    async fn dispatcher_forwards_non_control_messages() {
        let assistant = SdkMessage::Assistant {
            message: crate::messages::AssistantMessage::from_text("hello"),
            session_id: "sess".to_string(),
        };
        let callback = FakeCallback::with_responses(vec![]);
        let (lines, forwarded) = run_once(
            vec![assistant.clone()],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        assert!(
            lines.is_empty(),
            "non-control messages must not produce responses"
        );
        assert_eq!(forwarded.len(), 1);
        assert!(matches!(forwarded[0], SdkMessage::Assistant { .. }));
    }

    #[tokio::test]
    async fn dispatcher_hook_callback_replies_noop() {
        let req = SdkMessage::ControlRequest {
            request_id: "req_hook".to_string(),
            request: serde_json::json!({
                "subtype": "hook_callback",
                "callback_id": "cb_0",
                "input": {},
            }),
        };
        let callback = FakeCallback::with_responses(vec![]);
        let (lines, _forwarded) =
            run_once(vec![req], Some(callback as Arc<dyn PermissionCallback>)).await;
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req_hook");
        assert_eq!(parsed["response"]["response"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn dispatcher_unknown_subtype_returns_error() {
        let req = SdkMessage::ControlRequest {
            request_id: "req_x".to_string(),
            request: serde_json::json!({"subtype": "mcp_message"}),
        };
        let callback = FakeCallback::with_responses(vec![]);
        let (lines, _forwarded) =
            run_once(vec![req], Some(callback as Arc<dyn PermissionCallback>)).await;
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["subtype"], "error");
        assert!(
            parsed["response"]["error"]
                .as_str()
                .unwrap()
                .contains("mcp_message"),
            "expected error to mention subtype, got {parsed}"
        );
    }
}
