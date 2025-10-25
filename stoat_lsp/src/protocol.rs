//! JSON-RPC protocol message types and parsing.
//!
//! LSP uses JSON-RPC 2.0 for communication. This module provides types
//! for requests, responses, and notifications.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request message.
///
/// [LSP Specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#requestMessage)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// JSON-RPC version (always "2.0")
    pub jsonrpc: String,
    /// Request ID (can be number or string)
    pub id: RequestId,
    /// Method name (e.g., "textDocument/hover")
    pub method: String,
    /// Method parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC response message.
///
/// [LSP Specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#responseMessage)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// JSON-RPC version (always "2.0")
    pub jsonrpc: String,
    /// Request ID this response corresponds to
    pub id: RequestId,
    /// Response result (present on success)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Response error (present on failure)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// JSON-RPC notification message.
///
/// Notifications have no ID and expect no response.
///
/// [LSP Specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// JSON-RPC version (always "2.0")
    pub jsonrpc: String,
    /// Method name (e.g., "textDocument/didOpen")
    pub method: String,
    /// Method parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Request/response ID.
///
/// Can be either a number or a string per JSON-RPC 2.0.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        RequestId::String(s)
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Additional error data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes.
#[allow(unused)]
pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // LSP-specific error codes
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
    pub const UNKNOWN_ERROR_CODE: i32 = -32001;
    pub const REQUEST_FAILED: i32 = -32803;
    pub const SERVER_CANCELLED: i32 = -32802;
    pub const CONTENT_MODIFIED: i32 = -32801;
    pub const REQUEST_CANCELLED: i32 = -32800;
}

impl Request {
    /// Create a new request.
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl Response {
    /// Create a successful response.
    pub fn success(id: impl Into<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: impl Into<RequestId>, error: ResponseError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl Notification {
    /// Create a new notification.
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Parse a JSON-RPC message.
///
/// Determines if the message is a request, response, or notification
/// based on the presence of an `id` field and `result`/`error` fields.
pub fn parse_message(json: &str) -> Result<Message, serde_json::Error> {
    let value: Value = serde_json::from_str(json)?;

    if value.get("result").is_some() || value.get("error").is_some() {
        Ok(Message::Response(serde_json::from_value(value)?))
    } else if value.get("id").is_some() {
        Ok(Message::Request(serde_json::from_value(value)?))
    } else {
        Ok(Message::Notification(serde_json::from_value(value)?))
    }
}

/// A parsed JSON-RPC message.
#[derive(Debug, Clone)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialize_request() {
        let req = Request::new(1, "textDocument/hover", Some(json!({"key": "value"})));
        let json = req.to_json().expect("Failed to serialize");
        assert!(json.contains("\"method\":\"textDocument/hover\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn serialize_notification() {
        let notif = Notification::new("textDocument/didOpen", None);
        let json = notif.to_json().expect("Failed to serialize");
        assert!(json.contains("\"method\":\"textDocument/didOpen\""));
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn parse_request_message() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"test","params":{}}"#;
        let msg = parse_message(json).expect("Failed to parse");
        assert!(matches!(msg, Message::Request(_)));
    }

    #[test]
    fn parse_response_message() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let msg = parse_message(json).expect("Failed to parse");
        assert!(matches!(msg, Message::Response(_)));
    }

    #[test]
    fn parse_notification_message() {
        let json = r#"{"jsonrpc":"2.0","method":"test","params":{}}"#;
        let msg = parse_message(json).expect("Failed to parse");
        assert!(matches!(msg, Message::Notification(_)));
    }
}
