//! Demux of the ACP `session/update` streaming notification into the
//! host-facing [`AgentMessage`].
//!
//! The `update` object is kept as a [`Value`] and matched on its
//! `sessionUpdate` discriminator so unmodeled variants and fields are
//! tolerated rather than failing the whole stream.

use serde::Deserialize;
use serde_json::Value;
use stoat::host::AgentMessage;

pub(crate) const SESSION_UPDATE: &str = "session/update";

/// `session/update` params: the session the update belongs to and the
/// raw update object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionUpdateNotification {
    pub session_id: String,
    pub update: Value,
}

/// Map one `session/update` `update` onto an [`AgentMessage`], or `None`
/// for updates that carry no host-facing event: a user-message echo, an
/// unmodeled variant, or a chunk whose content is not text.
pub(crate) fn demux_session_update(update: &Value) -> Option<AgentMessage> {
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => Some(AgentMessage::Text {
            text: text_content(update)?,
        }),
        "agent_thought_chunk" => Some(AgentMessage::Thinking {
            text: text_content(update)?,
            signature: String::new(),
        }),
        "user_message_chunk" => None,
        _ => None,
    }
}

/// Extract the text of a chunk update's `content` content block, or
/// `None` when the block is not a text block.
fn text_content(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if content.get("type")?.as_str()? == "text" {
        content.get("text")?.as_str().map(str::to_string)
    } else {
        None
    }
}
