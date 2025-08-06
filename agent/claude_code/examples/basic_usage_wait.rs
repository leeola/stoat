use anyhow::Result;
use stoat_agent_claude_code::ClaudeCode;
use tokio::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

// Example: Using wait_for_response helper
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("stoat_agent_claude_code=debug,info")),
        )
        .init();

    let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

    // Send a message
    claude
        .send_message("What is the capital of France?")
        .await?;

    // Wait for response with timeout
    match claude.wait_for_response(Duration::from_secs(30)).await? {
        Some(response) => {
            info!("Got response: {}", response.content);
        },
        None => {
            info!("No response received within timeout");
        },
    }

    // Check if process is still alive
    if claude.is_alive() {
        info!("Claude is still running");
    }

    // Get session ID
    info!("Session ID: {}", claude.get_session_id());

    claude.shutdown().await?;
    Ok(())
}
