#[cfg(feature = "e2e_claude_code")]
mod e2e_tests {
    use std::time::Duration;
    use stoat::host::{AgentMessage, ClaudeCodeSession};
    use stoat_agent_claude_code::ClaudeCode;
    use tracing::info;
    use tracing_subscriber::EnvFilter;

    fn init_logging() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("stoat_agent_claude_code=trace")),
            )
            .with_test_writer()
            .try_init();
    }

    /// Pulls messages from the host until a terminal [`AgentMessage::Result`]
    /// or [`AgentMessage::Error`] arrives, concatenating any intermediate
    /// `Text` blocks. Skips `Init`, `ToolUse`, `ToolResult`, `Thinking`, and
    /// other non-text chatter. Returns `Err` on timeout, channel close, or
    /// an `Error` message.
    async fn collect_text_until_result(
        host: &dyn ClaudeCodeSession,
        total_timeout: Duration,
    ) -> Result<String, String> {
        let deadline = tokio::time::Instant::now() + total_timeout;
        let mut text = String::new();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {total_timeout:?} (partial text: {text:?})"
                ));
            }
            match tokio::time::timeout(remaining, host.recv()).await {
                Ok(Some(AgentMessage::Text { text: t })) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t);
                },
                Ok(Some(AgentMessage::Result { .. })) => return Ok(text),
                Ok(Some(AgentMessage::Error { message })) => {
                    return Err(format!("agent error: {message}"));
                },
                Ok(Some(_)) => {
                    // Init, ToolUse, ToolResult, Thinking, PartialText,
                    // ServerToolUse, ServerToolResult, Unknown - ignored
                    // for the text-collection goal.
                },
                Ok(None) => return Err("recv channel closed".to_string()),
                Err(_) => {
                    return Err(format!(
                        "timed out after {total_timeout:?} (partial text: {text:?})"
                    ));
                },
            }
        }
    }

    #[tokio::test]
    async fn test_basic_math_query() {
        init_logging();
        info!("Starting basic math query e2e test");

        let claude = ClaudeCode::builder()
            .model("sonnet")
            .build()
            .await
            .expect("Failed to spawn Claude");

        info!("Sending initial message");
        ClaudeCodeSession::send(&claude, "What is 2+2? Reply with just the number.")
            .await
            .expect("Failed to send message");

        info!("Collecting response (30s timeout)");
        let text = collect_text_until_result(&claude, Duration::from_secs(30))
            .await
            .expect("Failed to collect response");

        assert!(
            text.contains("4"),
            "Expected response to contain '4', got: {text}"
        );

        info!("Shutting down");
        ClaudeCodeSession::shutdown(&claude)
            .await
            .expect("Failed to shutdown");
    }

    #[tokio::test]
    async fn test_is_alive_and_shutdown() {
        init_logging();
        info!("Starting is_alive + shutdown e2e test");

        let claude = ClaudeCode::builder()
            .model("sonnet")
            .build()
            .await
            .expect("Failed to spawn Claude");

        info!("Sending message");
        ClaudeCodeSession::send(&claude, "What is 10+10? Reply with just the number.")
            .await
            .expect("Failed to send message");

        info!("Collecting response");
        let text = collect_text_until_result(&claude, Duration::from_secs(30))
            .await
            .expect("Failed to collect response");

        assert!(
            text.contains("20"),
            "Expected response to contain '20', got: {text}"
        );

        assert!(
            ClaudeCodeSession::is_alive(&claude),
            "Expected Claude to be alive before shutdown"
        );

        info!("Shutting down");
        ClaudeCodeSession::shutdown(&claude)
            .await
            .expect("Failed to shutdown");
    }

    #[tokio::test]
    async fn test_session_id_accessible() {
        init_logging();
        let claude = ClaudeCode::builder()
            .model("sonnet")
            .build()
            .await
            .expect("Failed to spawn Claude");

        let session_id = claude.get_session_id();
        assert!(!session_id.is_empty(), "Expected a non-empty session id");
        assert!(
            uuid::Uuid::parse_str(&session_id).is_ok(),
            "Expected session id to be a valid UUID, got: {session_id}"
        );

        ClaudeCodeSession::shutdown(&claude)
            .await
            .expect("Failed to shutdown");
    }
}
