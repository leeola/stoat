use std::io;

#[derive(Debug, Clone)]
pub enum AgentMessage {
    Init {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
    },
    Result {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
    Error {
        message: String,
    },
    /// Content block with an unrecognized `type` tag. Carries the raw
    /// JSON so consumers can surface or persist it without this crate
    /// needing to understand the schema.
    Unknown {
        raw: String,
    },
}

#[allow(async_fn_in_trait)]
pub trait ClaudeCodeHost: Send + Sync {
    async fn send(&self, content: &str) -> io::Result<()>;
    async fn recv(&self) -> Option<AgentMessage>;
    fn is_alive(&self) -> bool;
    async fn shutdown(&self) -> io::Result<()>;
}
