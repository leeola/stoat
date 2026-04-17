//! Multi-modal prompt builder.
//!
//! Accepts an ordered array of [`PromptChunk`]s (text, images,
//! resource links, embedded resources, audio) and renders them into the
//! single-user-message JSON blob shape the Claude Code CLI expects on
//! stdin. MCP-invoked slash commands (`/mcp:server:name ...`) are
//! rewritten to a human-readable `/server:name (MCP) ...` form before
//! hitting the wire.

use serde_json::json;

/// One chunk of a multi-modal user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptChunk {
    Text(String),
    Image {
        data: Option<String>,
        mime_type: String,
        uri: Option<String>,
    },
    ResourceLink {
        uri: String,
        name: Option<String>,
    },
    Resource {
        uri: String,
        text: Option<String>,
    },
    Audio {
        data: String,
        mime_type: String,
    },
}

/// Convert a slice of [`PromptChunk`]s into the wire-level user
/// message JSON the CLI expects. Returns a `serde_json::Value` rather
/// than a strongly-typed `UserMessage` because image and resource
/// content blocks are outside the current strongly-typed
/// `UserContentBlock` enum (which covers only text and tool results).
pub fn prompt_to_claude(chunks: &[PromptChunk]) -> serde_json::Value {
    let mut content: Vec<serde_json::Value> = Vec::new();
    let mut context: Vec<serde_json::Value> = Vec::new();

    for chunk in chunks {
        match chunk {
            PromptChunk::Text(text) => {
                content.push(json!({
                    "type": "text",
                    "text": normalize_mcp_slash_command(text),
                }));
            },
            PromptChunk::Image {
                data,
                mime_type,
                uri,
            } => {
                if let Some(d) = data {
                    content.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": d,
                        }
                    }));
                } else if let Some(u) = uri
                    && u.starts_with("http")
                {
                    // file:// and other URIs are silently skipped: the CLI
                    // has no generic URI fetcher, so only data and http(s)
                    // URIs make sense to forward.
                    content.push(json!({
                        "type": "image",
                        "source": {"type": "url", "url": u},
                    }));
                }
            },
            PromptChunk::ResourceLink { uri, name } => {
                content.push(json!({
                    "type": "text",
                    "text": format_uri_as_link(uri, name.as_deref()),
                }));
            },
            PromptChunk::Resource { uri, text } => {
                content.push(json!({
                    "type": "text",
                    "text": format_uri_as_link(uri, None),
                }));
                if let Some(t) = text {
                    context.push(json!({
                        "type": "text",
                        "text": format!("<context ref=\"{uri}\">{t}</context>"),
                    }));
                }
            },
            PromptChunk::Audio { .. } => {
                // Not supported by the CLI today; drop silently.
            },
        }
    }

    content.extend(context);

    json!({
        "role": "user",
        "content": content,
    })
}

/// Render a URI as a markdown-style link. `name` falls back to the
/// URI's trailing segment or the URI itself.
pub fn format_uri_as_link(uri: &str, name: Option<&str>) -> String {
    let label = name
        .map(|s| s.to_string())
        .unwrap_or_else(|| uri.rsplit('/').next().unwrap_or(uri).to_string());
    format!("[@{label}]({uri})")
}

/// Rewrite `/mcp:server:command args...` into the human-readable form
/// (`/server:command (MCP) args...`) that surfaces the MCP server
/// origin in the chat transcript.
pub fn normalize_mcp_slash_command(text: &str) -> String {
    let Some(rest) = text.strip_prefix("/mcp:") else {
        return text.to_string();
    };
    // Split on the first whitespace into `command_token` and `args`.
    let (cmd, args) = match rest.split_once(char::is_whitespace) {
        Some((c, a)) => (c, Some(a)),
        None => (rest, None),
    };
    match args {
        Some(args) => format!("/{cmd} (MCP) {args}"),
        None => format!("/{cmd} (MCP)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_chunks_pass_through() {
        let raw = prompt_to_claude(&[PromptChunk::Text("hello".into())]);
        assert_eq!(raw["content"][0]["type"], "text");
        assert_eq!(raw["content"][0]["text"], "hello");
    }

    #[test]
    fn image_with_base64_data_preserves_source_type() {
        let raw = prompt_to_claude(&[PromptChunk::Image {
            data: Some("xyz".into()),
            mime_type: "image/png".into(),
            uri: None,
        }]);
        assert_eq!(raw["content"][0]["type"], "image");
        assert_eq!(raw["content"][0]["source"]["type"], "base64");
        assert_eq!(raw["content"][0]["source"]["data"], "xyz");
    }

    #[test]
    fn resource_with_text_emits_context_tail() {
        let raw = prompt_to_claude(&[PromptChunk::Resource {
            uri: "file:///tmp/a".into(),
            text: Some("body".into()),
        }]);
        let content = raw["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .contains("<context ref=\"file:///tmp/a\">body</context>")
        );
    }

    #[test]
    fn mcp_slash_command_is_normalised() {
        let text = "/mcp:weather:forecast SF";
        assert_eq!(
            normalize_mcp_slash_command(text),
            "/weather:forecast (MCP) SF"
        );
    }

    #[test]
    fn non_mcp_slash_command_untouched() {
        assert_eq!(normalize_mcp_slash_command("/compact"), "/compact");
    }

    #[test]
    fn format_uri_uses_basename_when_no_name() {
        assert_eq!(
            format_uri_as_link("file:///a/b/c.rs", None),
            "[@c.rs](file:///a/b/c.rs)"
        );
    }
}
