//! Agent->client `session/request_permission` handler: surfaces the
//! request through Stoat's permission UI (an injected
//! `mpsc::Sender<PermissionPrompt>`) and maps the user's
//! [`ApprovalDecision`] back to the ACP option the agent offered.

use crate::rpc::{error, parse_params, INTERNAL_ERROR};
use serde::Deserialize;
use serde_json::{json, Value};
use stoat::host::{ApprovalDecision, PermissionPrompt};
use stoat_agent_claude_code::jsonrpc::{IncomingRequest, RpcError};
use tokio::sync::{mpsc, oneshot};

pub(crate) const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";

/// Answer one `session/request_permission` request: prompt the user
/// through `permission_tx`, then respond with the option they selected
/// (or a cancellation). Awaits the user, so it is meant to be spawned.
pub(crate) async fn handle_permission_request(
    req: IncomingRequest,
    permission_tx: mpsc::Sender<PermissionPrompt>,
) {
    let response = request_permission(req.params.as_ref(), &permission_tx).await;
    let _ = req.respond(response);
}

async fn request_permission(
    params: Option<&Value>,
    permission_tx: &mpsc::Sender<PermissionPrompt>,
) -> Result<Value, RpcError> {
    let params: RequestPermissionParams = parse_params(params)?;

    let (response_tx, response_rx) = oneshot::channel();
    let prompt = PermissionPrompt {
        tool: params.tool_call.display_name(),
        input: params.tool_call.input_string(),
        response_tx,
    };
    permission_tx
        .send(prompt)
        .await
        .map_err(|_| error(INTERNAL_ERROR, "permission channel closed"))?;

    // A dropped response sender (the modal was dismissed without a
    // choice) is a cancellation, not an error.
    let outcome = match response_rx.await {
        Ok(decision) => match decision_to_option(decision, &params.options) {
            Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
            None => json!({ "outcome": "cancelled" }),
        },
        Err(_) => json!({ "outcome": "cancelled" }),
    };
    Ok(json!({ "outcome": outcome }))
}

/// Pick the option id whose `kind` best matches the user's decision: the
/// first option matching a kind in the decision's preference order. No
/// compatible option yields `None`, surfaced to the agent as a
/// cancellation.
fn decision_to_option(decision: ApprovalDecision, options: &[PermissionOption]) -> Option<String> {
    let preferred: &[&str] = match decision {
        ApprovalDecision::Allow | ApprovalDecision::AllowOnce => &["allow_once", "allow_always"],
        ApprovalDecision::AlwaysAllow => &["allow_always", "allow_once"],
        ApprovalDecision::Deny => &["reject_once", "reject_always"],
    };
    preferred.iter().find_map(|kind| {
        options
            .iter()
            .find(|option| option.kind == *kind)
            .map(|option| option.option_id.clone())
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestPermissionParams {
    tool_call: ToolCallRef,
    #[serde(default)]
    options: Vec<PermissionOption>,
}

/// The agent's tool call, as far as the prompt needs it: a name to show
/// and its raw input. Other fields on the wire are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallRef {
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    raw_input: Option<Value>,
}

impl ToolCallRef {
    fn display_name(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| self.tool_call_id.clone())
    }

    fn input_string(&self) -> String {
        match &self.raw_input {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption {
    option_id: String,
    #[serde(default)]
    kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<PermissionOption> {
        ["allow_once", "allow_always", "reject_once"]
            .into_iter()
            .map(|kind| PermissionOption {
                option_id: kind.to_string(),
                kind: kind.to_string(),
            })
            .collect()
    }

    #[test]
    fn maps_decision_to_matching_option_kind() {
        let opts = options();
        let pick = |d| decision_to_option(d, &opts);
        assert_eq!(
            pick(ApprovalDecision::AllowOnce).as_deref(),
            Some("allow_once")
        );
        assert_eq!(pick(ApprovalDecision::Allow).as_deref(), Some("allow_once"));
        assert_eq!(
            pick(ApprovalDecision::AlwaysAllow).as_deref(),
            Some("allow_always")
        );
        assert_eq!(pick(ApprovalDecision::Deny).as_deref(), Some("reject_once"));
    }

    #[test]
    fn falls_back_within_preference_order() {
        let opts = vec![PermissionOption {
            option_id: "x".to_string(),
            kind: "allow_always".to_string(),
        }];
        assert_eq!(
            decision_to_option(ApprovalDecision::AllowOnce, &opts).as_deref(),
            Some("x")
        );
    }

    #[test]
    fn no_compatible_option_is_none() {
        let opts = vec![PermissionOption {
            option_id: "x".to_string(),
            kind: "reject_once".to_string(),
        }];
        assert_eq!(decision_to_option(ApprovalDecision::AllowOnce, &opts), None);
    }
}
