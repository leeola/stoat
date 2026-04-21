//! Control-protocol request/response types.
//!
//! Strongly-typed views of the `control_request` payloads the CLI sends
//! (permission prompts, hook callbacks), plus the outbound
//! [`ControlResponse`] the host writes back on stdin.

use serde::Serialize;

/// Strongly-typed view of a `can_use_tool` control request, borrowed
/// from the underlying `SdkMessage::ControlRequest`'s raw JSON.
#[derive(Debug, Clone, Copy)]
pub struct CanUseToolRequest<'a> {
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub input: &'a serde_json::Value,
    pub permission_suggestions: Option<&'a serde_json::Value>,
    pub tool_use_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub blocked_path: Option<&'a str>,
}

/// Strongly-typed view of a `hook_callback` control request.
#[derive(Debug, Clone, Copy)]
pub struct HookCallbackRequest<'a> {
    pub request_id: &'a str,
    pub callback_id: &'a str,
    pub input: Option<&'a serde_json::Value>,
    pub tool_use_id: Option<&'a str>,
}

/// Outbound control-protocol response. Serialized once and written to
/// child stdin; never inbound. The top-level `type` is
/// `"control_response"`; the inner `response` carries a success or
/// error subtype plus the original `request_id`.
#[derive(Debug, Clone, Serialize)]
pub struct ControlResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    pub response: ControlResponseBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlResponseBody {
    Success {
        request_id: String,
        response: serde_json::Value,
    },
    Error {
        request_id: String,
        error: String,
    },
}

impl ControlResponse {
    pub fn success(request_id: impl Into<String>, response: serde_json::Value) -> Self {
        Self {
            kind: "control_response",
            response: ControlResponseBody::Success {
                request_id: request_id.into(),
                response,
            },
        }
    }

    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            kind: "control_response",
            response: ControlResponseBody::Error {
                request_id: request_id.into(),
                error: error.into(),
            },
        }
    }
}
