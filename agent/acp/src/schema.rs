//! Hand-defined subset of the ACP JSON-RPC message schema: the request
//! params/results and notification params for the initialize/session
//! lifecycle. Fields are camelCase on the wire and serialize into the
//! transport's opaque JSON values. Only the fields this crate reads are
//! modeled; unknown fields a future agent adds deserialize away.

use serde::{Deserialize, Serialize};

/// ACP protocol version advertised in [`InitializeParams`].
pub(crate) const PROTOCOL_VERSION: u32 = 1;

pub(crate) const INITIALIZE: &str = "initialize";
pub(crate) const SESSION_NEW: &str = "session/new";
pub(crate) const SESSION_PROMPT: &str = "session/prompt";
pub(crate) const SESSION_CANCEL: &str = "session/cancel";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionParams {
    pub cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionResult {
    pub session_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// A prompt content block. Only the text variant is modeled; the wire
/// shape is `{"type": "text", "text": "..."}`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ContentBlock {
    Text { text: String },
}

impl ContentBlock {
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self::Text {
            text: content.into(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelParams {
    pub session_id: String,
}
