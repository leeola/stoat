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

use crate::{
    messages::{CanUseToolRequest, ControlResponse, HookCallbackRequest, SdkMessage, ToolUse},
    tools::tool_info_from_tool_use,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
};
use stoat::host::{
    AgentMessage, HookCallback, HookDecision, HookEvent, HookKind, HookResponse,
    PermissionBehavior, PermissionCallback, PermissionDestination, PermissionResult,
    PermissionRule, PermissionScope, PermissionSuggestion, ToolCallStatus, ToolPermissionContext,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Internal hook wrapper that intercepts well-known `PostToolUse`
/// events (specifically `EnterPlanMode` and Edit-family tools) and
/// synthesises matching host-level [`AgentMessage`]s into the shared
/// pending queue. When a user-registered `HookCallback` is attached,
/// its response is returned verbatim; otherwise a default
/// "allow to continue" response is returned.
pub(crate) struct DefaultHookCallback {
    pub(crate) pending: Arc<StdMutex<VecDeque<AgentMessage>>>,
    pub(crate) inner: Option<Arc<dyn HookCallback>>,
}

#[async_trait::async_trait]
impl HookCallback for DefaultHookCallback {
    async fn handle_hook(&self, event: HookEvent<'_>) -> HookResponse {
        // Only PostToolUse carries the tool_response we care about.
        if event.kind() == HookKind::PostToolUse {
            self.intercept_post_tool_use(&event);
        }

        if let Some(inner) = &self.inner {
            inner.handle_hook(event).await
        } else {
            HookResponse::r#continue()
        }
    }
}

impl DefaultHookCallback {
    fn intercept_post_tool_use(&self, event: &HookEvent<'_>) {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload_json) else {
            return;
        };
        let tool_name = payload
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match tool_name {
            "EnterPlanMode" => {
                self.push(AgentMessage::ModeChanged {
                    mode: "plan".to_string(),
                });
            },
            "Edit" | "Write" | "MultiEdit" => {
                if let Some(response) = payload.get("tool_response") {
                    let update = crate::tools::tool_update_from_edit_tool_response(response);
                    if let Some(content) = update.content {
                        let id = event.tool_use_id.unwrap_or("").to_string();
                        self.push(AgentMessage::ToolUpdate {
                            id,
                            content,
                            status: update.status.unwrap_or(ToolCallStatus::Completed),
                        });
                    }
                }
            },
            _ => {},
        }
    }

    fn push(&self, msg: AgentMessage) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.push_back(msg);
        }
    }
}

/// Correlates outgoing `control_request` frames with inbound
/// `control_response` frames so the sender can await acknowledgement.
/// Keyed by the `request_id` field of the original request.
pub(crate) type ControlWaiters =
    Arc<StdMutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<ControlAck>>>>;

/// Result delivered to a correlator-awaiting caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlAck {
    /// CLI replied `{ subtype: "success", ... }`.
    Success(serde_json::Value),
    /// CLI replied `{ subtype: "error", error: <message> }`.
    Error(String),
}

/// Dependencies the dispatcher holds for the lifetime of the session.
pub(crate) struct DispatcherDeps {
    pub permission_callback: Option<Arc<dyn PermissionCallback>>,
    pub hook_callback: Option<Arc<dyn HookCallback>>,
    pub stdin_tx: mpsc::Sender<String>,
    pub control_waiters: ControlWaiters,
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
            SdkMessage::ControlResponse { .. } => {
                handle_control_response(&msg, &deps);
                // Responses are routed to waiters and dropped.
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

fn handle_control_response(msg: &SdkMessage, deps: &DispatcherDeps) {
    let Some((request_id, subtype, body)) = msg.as_control_response() else {
        return;
    };
    let request_id = request_id.to_string();
    let ack = match subtype {
        "success" => {
            let inner = body
                .get("response")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            ControlAck::Success(inner)
        },
        "error" => {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            ControlAck::Error(err)
        },
        other => ControlAck::Error(format!("unknown control_response subtype: {other}")),
    };

    let waiter = {
        let Ok(mut guard) = deps.control_waiters.lock() else {
            warn!("dispatcher: control_waiters mutex poisoned");
            return;
        };
        guard.remove(&request_id)
    };
    match waiter {
        Some(tx) => {
            if tx.send(ack).is_err() {
                debug!(
                    "dispatcher: control_response waiter for {request_id} dropped before receive"
                );
            }
        },
        None => {
            debug!("dispatcher: control_response for unknown request_id {request_id}");
        },
    }
}

async fn handle_control_request(msg: &SdkMessage, deps: &DispatcherDeps) {
    if let Some(req) = msg.as_can_use_tool() {
        dispatch_can_use_tool(req, deps).await;
        return;
    }
    if let Some(req) = msg.as_hook_callback() {
        dispatch_hook_callback(req, deps).await;
        return;
    }

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

    // Any other control subtype (mcp_message, or anything the CLI adds
    // in future releases) gets a success-empty reply so the CLI doesn't
    // stall waiting for a real answer. Replying `error` would abort MCP
    // / oauth / elicitation flows that the CLI treats as best-effort.
    debug!(
        "dispatcher: control_request subtype '{}' (request_id {}) -> default success reply",
        subtype, request_id
    );
    let response = ControlResponse::success(request_id, serde_json::json!({}));
    send_response(response, deps).await;
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

    // Enrich the permission context with classifier output (kind, title,
    // display content, locations) before invoking the host's
    // PermissionCallback, so the host can render a decorated prompt.
    let tool_use = ToolUse {
        id: req.tool_use_id.unwrap_or("").to_string(),
        name: req.tool_name.to_string(),
        input: serde_json::from_value(req.input.clone()).unwrap_or_default(),
    };
    let info = tool_info_from_tool_use(&tool_use, false, None);

    let context = ToolPermissionContext {
        suggestions_json: suggestions_json.as_deref(),
        tool_use_id: req.tool_use_id,
        agent_id: req.agent_id,
        blocked_path: req.blocked_path,
        tool_kind: info.kind,
        tool_title: info.title,
        tool_content: info.content,
        tool_locations: info.locations,
    };

    let request_id = req.request_id.to_string();
    let original_input = req.input.clone();

    let result = callback
        .can_use_tool(req.tool_name, &input_json, context)
        .await;

    let body = match result {
        PermissionResult::Allow {
            scope,
            updated_input_json,
            updated_permissions,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("behavior".to_string(), "allow".into());
            let updated_input_value = match updated_input_json {
                Some(s) => serde_json::from_str::<serde_json::Value>(&s).unwrap_or(original_input),
                None => original_input,
            };
            map.insert("updatedInput".to_string(), updated_input_value);

            // Serialize PermissionSuggestions (SetMode / AddRules) into
            // the `updatedPermissions` array shape the CLI consumes in
            // its control_response. If the host provided no explicit
            // suggestions and asked for `Always` scope, synthesise a
            // default AddRules entry so the CLI remembers the approval.
            let suggestions = if updated_permissions.is_empty() && scope == PermissionScope::Always
            {
                vec![PermissionSuggestion::AddRules {
                    rules: vec![PermissionRule {
                        tool_name: Some(req.tool_name.to_string()),
                        input_pattern: None,
                    }],
                    behavior: PermissionBehavior::Allow,
                    destination: PermissionDestination::Session,
                }]
            } else {
                updated_permissions
            };
            if !suggestions.is_empty() {
                map.insert(
                    "updatedPermissions".to_string(),
                    serde_json::Value::Array(
                        suggestions
                            .into_iter()
                            .map(permission_suggestion_to_json)
                            .collect(),
                    ),
                );
            }
            // Scope is an internal concept on the host side that the
            // CLI doesn't understand directly; we encode it under
            // `_meta.scope` for debuggability.
            map.insert(
                "_meta".to_string(),
                serde_json::json!({ "scope": scope_as_str(scope) }),
            );
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
        PermissionResult::Cancel => {
            let mut map = serde_json::Map::new();
            map.insert("behavior".to_string(), "deny".into());
            map.insert(
                "message".to_string(),
                "User cancelled the permission prompt.".into(),
            );
            map.insert(
                "_meta".to_string(),
                serde_json::json!({ "cancelled": true }),
            );
            serde_json::Value::Object(map)
        },
    };

    send_response(ControlResponse::success(request_id, body), deps).await;
}

fn scope_as_str(scope: PermissionScope) -> &'static str {
    match scope {
        PermissionScope::Once => "once",
        PermissionScope::Session => "session",
        PermissionScope::Always => "always",
    }
}

fn destination_as_str(d: PermissionDestination) -> &'static str {
    match d {
        PermissionDestination::Session => "session",
        PermissionDestination::Project => "project",
        PermissionDestination::User => "user",
    }
}

fn behavior_as_str(b: PermissionBehavior) -> &'static str {
    match b {
        PermissionBehavior::Allow => "allow",
        PermissionBehavior::Deny => "deny",
        PermissionBehavior::Ask => "ask",
    }
}

fn permission_suggestion_to_json(s: PermissionSuggestion) -> serde_json::Value {
    match s {
        PermissionSuggestion::SetMode { mode, destination } => serde_json::json!({
            "type": "setMode",
            "mode": mode,
            "destination": destination_as_str(destination),
        }),
        PermissionSuggestion::AddRules {
            rules,
            behavior,
            destination,
        } => {
            let rules_json: Vec<serde_json::Value> = rules
                .into_iter()
                .map(|r| {
                    let mut rule = serde_json::Map::new();
                    if let Some(tn) = r.tool_name {
                        rule.insert("toolName".to_string(), serde_json::Value::String(tn));
                    }
                    if let Some(pat) = r.input_pattern {
                        rule.insert("inputPattern".to_string(), serde_json::Value::String(pat));
                    }
                    serde_json::Value::Object(rule)
                })
                .collect();
            serde_json::json!({
                "type": "addRules",
                "rules": rules_json,
                "behavior": behavior_as_str(behavior),
                "destination": destination_as_str(destination),
            })
        },
    }
}

async fn dispatch_hook_callback(req: HookCallbackRequest<'_>, deps: &DispatcherDeps) {
    // Without a registered host-side callback, reply success-empty so
    // the CLI doesn't stall. This matches the pre-hook-system behaviour
    // for the no-op path.
    let Some(callback) = deps.hook_callback.clone() else {
        debug!(
            "dispatcher: hook_callback {} (tool_use_id {:?}) -> no-op reply (no host callback)",
            req.callback_id, req.tool_use_id
        );
        let response = ControlResponse::success(req.request_id, serde_json::json!({}));
        send_response(response, deps).await;
        return;
    };

    // The CLI puts the hook's event name either as a top-level field on
    // the control request or inside `input.hook_event_name`. Check both.
    let hook_event_name = req
        .input
        .and_then(|v| v.get("hook_event_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let payload_json = req
        .input
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());

    let event = HookEvent {
        kind_name: hook_event_name,
        payload_json: &payload_json,
        tool_use_id: req.tool_use_id,
        callback_id: req.callback_id,
    };

    let response = callback.handle_hook(event).await;
    let body = hook_response_to_json(response);
    send_response(ControlResponse::success(req.request_id, body), deps).await;
}

fn hook_response_to_json(resp: HookResponse) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("continue".to_string(), resp.r#continue.into());
    if let Some(decision) = resp.decision {
        match decision {
            HookDecision::Allow { reason } => {
                map.insert(
                    "decision".to_string(),
                    serde_json::Value::String("allow".into()),
                );
                if let Some(r) = reason {
                    map.insert("reason".to_string(), serde_json::Value::String(r));
                }
            },
            HookDecision::Block { reason } => {
                map.insert(
                    "decision".to_string(),
                    serde_json::Value::String("block".into()),
                );
                map.insert("reason".to_string(), serde_json::Value::String(reason));
            },
            HookDecision::Modify { updated_input_json } => {
                map.insert(
                    "decision".to_string(),
                    serde_json::Value::String("modify".into()),
                );
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&updated_input_json) {
                    map.insert("updatedInput".to_string(), v);
                }
            },
        }
    }
    if let Some(ho) = resp.hook_specific_output_json
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&ho)
    {
        map.insert("hookSpecificOutput".to_string(), v);
    }
    serde_json::Value::Object(map)
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
            hook_callback: None,
            stdin_tx,
            control_waiters: Arc::new(StdMutex::new(std::collections::HashMap::new())),
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
            parent_tool_use_id: None,
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
    async fn dispatcher_allow_scope_serialized_in_meta() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::allow_with_scope(
            PermissionScope::Session,
        )]);
        let (lines, _) = run_once(
            vec![control_request_can_use_tool("req_s", "Read", "/tmp")],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["response"]["behavior"], "allow");
        assert_eq!(parsed["response"]["response"]["_meta"]["scope"], "session");
    }

    #[tokio::test]
    async fn dispatcher_allow_always_synthesises_default_rule() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::allow_with_scope(
            PermissionScope::Always,
        )]);
        let (lines, _) = run_once(
            vec![control_request_can_use_tool("req_a", "Read", "/tmp")],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let suggestions = &parsed["response"]["response"]["updatedPermissions"];
        assert!(suggestions.is_array());
        assert_eq!(suggestions[0]["type"], "addRules");
        assert_eq!(suggestions[0]["behavior"], "allow");
        assert_eq!(suggestions[0]["rules"][0]["toolName"], "Read");
    }

    #[tokio::test]
    async fn dispatcher_cancel_serialises_as_deny_with_meta() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::Cancel]);
        let (lines, _) = run_once(
            vec![control_request_can_use_tool("req_c", "Bash", "pwd")],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["response"]["behavior"], "deny");
        assert_eq!(parsed["response"]["response"]["_meta"]["cancelled"], true);
    }

    #[tokio::test]
    async fn dispatcher_updated_permissions_passed_through() {
        let callback = FakeCallback::with_responses(vec![PermissionResult::Allow {
            scope: PermissionScope::Once,
            updated_input_json: None,
            updated_permissions: vec![PermissionSuggestion::SetMode {
                mode: "acceptEdits".into(),
                destination: PermissionDestination::Session,
            }],
        }]);
        let (lines, _) = run_once(
            vec![control_request_can_use_tool("req_u", "Edit", "x")],
            Some(callback as Arc<dyn PermissionCallback>),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let suggestions = &parsed["response"]["response"]["updatedPermissions"];
        assert_eq!(suggestions[0]["type"], "setMode");
        assert_eq!(suggestions[0]["mode"], "acceptEdits");
        assert_eq!(suggestions[0]["destination"], "session");
    }

    #[tokio::test]
    async fn dispatcher_unknown_subtype_returns_success_empty_reply() {
        // The CLI emits subtypes the dispatcher doesn't explicitly
        // model (mcp_message, elicitation, oauth_token_refresh, ...).
        // Replying with `error` stalls those flows, so the dispatcher
        // returns `success` with an empty body instead.
        let req = SdkMessage::ControlRequest {
            request_id: "req_x".to_string(),
            request: serde_json::json!({"subtype": "mcp_message"}),
        };
        let callback = FakeCallback::with_responses(vec![]);
        let (lines, _forwarded) =
            run_once(vec![req], Some(callback as Arc<dyn PermissionCallback>)).await;
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req_x");
        assert_eq!(parsed["response"]["response"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn dispatcher_hook_callback_routes_to_registered_host_callback() {
        use async_trait::async_trait;
        use stoat::host::{HookCallback, HookDecision, HookEvent, HookResponse};

        struct CaptureHook {
            seen: Mutex<Vec<(String, String)>>,
        }
        #[async_trait]
        impl HookCallback for CaptureHook {
            async fn handle_hook(&self, event: HookEvent<'_>) -> HookResponse {
                self.seen
                    .lock()
                    .unwrap()
                    .push((event.kind_name.to_string(), event.payload_json.to_string()));
                HookResponse {
                    r#continue: true,
                    decision: Some(HookDecision::Allow {
                        reason: Some("ok".into()),
                    }),
                    hook_specific_output_json: None,
                }
            }
        }
        let hook = Arc::new(CaptureHook {
            seen: Mutex::new(Vec::new()),
        });

        let (inner_tx, inner_rx) = mpsc::channel::<SdkMessage>(4);
        let (outer_tx, _outer_rx) = mpsc::channel::<SdkMessage>(4);
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(4);

        inner_tx
            .send(SdkMessage::ControlRequest {
                request_id: "req_hk".into(),
                request: serde_json::json!({
                    "subtype": "hook_callback",
                    "callback_id": "cb_1",
                    "input": {"hook_event_name": "PostToolUse", "tool_use_id": "toolu_x"},
                    "tool_use_id": "toolu_x",
                }),
            })
            .await
            .unwrap();
        drop(inner_tx);

        let deps = DispatcherDeps {
            permission_callback: None,
            hook_callback: Some(hook.clone() as Arc<dyn HookCallback>),
            stdin_tx,
            control_waiters: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        };
        run_dispatcher(inner_rx, outer_tx, deps).await;

        let reply = stdin_rx.try_recv().expect("reply written");
        let parsed: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["response"]["continue"], true);
        assert_eq!(parsed["response"]["response"]["decision"], "allow");
        assert_eq!(parsed["response"]["response"]["reason"], "ok");

        let seen = hook.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "PostToolUse");
    }

    // ---- DefaultHookCallback ----

    #[tokio::test]
    async fn default_hook_emits_mode_changed_on_enter_plan_mode() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let cb = DefaultHookCallback {
            pending: pending.clone(),
            inner: None,
        };
        let payload = serde_json::json!({
            "tool_name": "EnterPlanMode",
            "tool_use_id": "toolu_1",
        })
        .to_string();
        let event = HookEvent {
            kind_name: "PostToolUse",
            payload_json: &payload,
            tool_use_id: Some("toolu_1"),
            callback_id: "cb_0",
        };
        let resp = cb.handle_hook(event).await;
        assert!(resp.r#continue);
        let queued = pending.lock().unwrap();
        assert_eq!(queued.len(), 1);
        match &queued[0] {
            AgentMessage::ModeChanged { mode } => assert_eq!(mode, "plan"),
            other => panic!("expected ModeChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_hook_emits_tool_update_on_edit_with_structured_patch() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let cb = DefaultHookCallback {
            pending: pending.clone(),
            inner: None,
        };
        let payload = serde_json::json!({
            "tool_name": "Edit",
            "tool_use_id": "toolu_edit_1",
            "tool_response": {
                "filePath": "/tmp/x.rs",
                "structuredPatch": [
                    {
                        "oldLines": ["foo"],
                        "newLines": ["bar"],
                    }
                ]
            }
        })
        .to_string();
        let event = HookEvent {
            kind_name: "PostToolUse",
            payload_json: &payload,
            tool_use_id: Some("toolu_edit_1"),
            callback_id: "cb_0",
        };
        cb.handle_hook(event).await;
        let queued = pending.lock().unwrap();
        match queued.front() {
            Some(AgentMessage::ToolUpdate {
                id,
                content,
                status,
            }) => {
                assert_eq!(id, "toolu_edit_1");
                assert_eq!(*status, ToolCallStatus::Completed);
                assert_eq!(content.len(), 1);
                match &content[0] {
                    stoat::host::ToolCallContent::Diff {
                        path,
                        old_text,
                        new_text,
                    } => {
                        assert_eq!(path.to_string_lossy(), "/tmp/x.rs");
                        assert_eq!(old_text.as_deref(), Some("foo"));
                        assert_eq!(new_text, "bar");
                    },
                    other => panic!("expected Diff content, got {other:?}"),
                }
            },
            other => panic!("expected ToolUpdate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_hook_leaves_non_intercepted_events_alone() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let cb = DefaultHookCallback {
            pending: pending.clone(),
            inner: None,
        };
        // Bash PostToolUse is not one of the intercepted tools.
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_use_id": "toolu_b",
            "tool_response": {"output": "hello"}
        })
        .to_string();
        let event = HookEvent {
            kind_name: "PostToolUse",
            payload_json: &payload,
            tool_use_id: Some("toolu_b"),
            callback_id: "cb_0",
        };
        cb.handle_hook(event).await;
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn default_hook_delegates_to_inner_callback() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Tracker(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl HookCallback for Tracker {
            async fn handle_hook(&self, _event: HookEvent<'_>) -> HookResponse {
                self.0.store(true, Ordering::SeqCst);
                HookResponse::block("inner decision")
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(Tracker(flag.clone()));
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let cb = DefaultHookCallback {
            pending,
            inner: Some(inner as Arc<dyn HookCallback>),
        };
        let event = HookEvent {
            kind_name: "UserPromptSubmit",
            payload_json: "{}",
            tool_use_id: None,
            callback_id: "cb",
        };
        let resp = cb.handle_hook(event).await;
        assert!(flag.load(Ordering::SeqCst), "inner callback must fire");
        // Inner's decision propagates verbatim.
        assert!(!resp.r#continue);
    }

    // ---- Control-response correlator ----

    #[tokio::test]
    async fn correlator_routes_success_to_waiter() {
        let waiters: ControlWaiters = Arc::new(StdMutex::new(std::collections::HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        waiters.lock().unwrap().insert("req_42".to_string(), tx);
        let deps = DispatcherDeps {
            permission_callback: None,
            hook_callback: None,
            stdin_tx: mpsc::channel::<String>(1).0,
            control_waiters: waiters.clone(),
        };
        let frame = SdkMessage::ControlResponse {
            response: serde_json::json!({
                "subtype": "success",
                "request_id": "req_42",
                "response": {"ack": true}
            }),
        };
        handle_control_response(&frame, &deps);
        match rx.await {
            Ok(ControlAck::Success(body)) => assert_eq!(body["ack"], true),
            other => panic!("expected Success, got {other:?}"),
        }
        assert!(waiters.lock().unwrap().is_empty(), "waiter must be removed");
    }

    #[tokio::test]
    async fn correlator_routes_error_to_waiter() {
        let waiters: ControlWaiters = Arc::new(StdMutex::new(std::collections::HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        waiters.lock().unwrap().insert("req_err".to_string(), tx);
        let deps = DispatcherDeps {
            permission_callback: None,
            hook_callback: None,
            stdin_tx: mpsc::channel::<String>(1).0,
            control_waiters: waiters.clone(),
        };
        let frame = SdkMessage::ControlResponse {
            response: serde_json::json!({
                "subtype": "error",
                "request_id": "req_err",
                "error": "bad request",
            }),
        };
        handle_control_response(&frame, &deps);
        match rx.await {
            Ok(ControlAck::Error(msg)) => assert_eq!(msg, "bad request"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn correlator_drops_response_without_matching_waiter() {
        let waiters: ControlWaiters = Arc::new(StdMutex::new(std::collections::HashMap::new()));
        let deps = DispatcherDeps {
            permission_callback: None,
            hook_callback: None,
            stdin_tx: mpsc::channel::<String>(1).0,
            control_waiters: waiters.clone(),
        };
        let frame = SdkMessage::ControlResponse {
            response: serde_json::json!({
                "subtype": "success",
                "request_id": "req_orphan",
                "response": {}
            }),
        };
        handle_control_response(&frame, &deps);
        // No waiters; no panic; no state mutation.
        assert!(waiters.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn correlator_forwards_non_control_messages_unchanged() {
        let (inner_tx, inner_rx) = mpsc::channel::<SdkMessage>(4);
        let (outer_tx, mut outer_rx) = mpsc::channel::<SdkMessage>(4);
        let (stdin_tx, _stdin_rx) = mpsc::channel::<String>(4);

        // Push a ControlResponse through the dispatcher. It must NOT
        // reach outer_rx (dispatcher consumes it).
        inner_tx
            .send(SdkMessage::ControlResponse {
                response: serde_json::json!({
                    "subtype": "success",
                    "request_id": "req_x",
                    "response": {}
                }),
            })
            .await
            .unwrap();
        // And a non-control message that MUST pass through.
        inner_tx
            .send(SdkMessage::Assistant {
                message: crate::messages::AssistantMessage::from_text("hi"),
                session_id: "sess".into(),
                parent_tool_use_id: None,
            })
            .await
            .unwrap();
        drop(inner_tx);

        let deps = DispatcherDeps {
            permission_callback: None,
            hook_callback: None,
            stdin_tx,
            control_waiters: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        };
        run_dispatcher(inner_rx, outer_tx, deps).await;

        let mut seen = Vec::new();
        while let Ok(msg) = outer_rx.try_recv() {
            seen.push(msg);
        }
        assert_eq!(seen.len(), 1, "only the Assistant should forward");
        assert!(matches!(seen[0], SdkMessage::Assistant { .. }));
    }

    #[tokio::test]
    async fn default_hook_default_response_when_no_inner() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let cb = DefaultHookCallback {
            pending,
            inner: None,
        };
        let event = HookEvent {
            kind_name: "SessionStart",
            payload_json: "{}",
            tool_use_id: None,
            callback_id: "cb",
        };
        let resp = cb.handle_hook(event).await;
        assert!(resp.r#continue);
        assert!(resp.decision.is_none());
    }
}
