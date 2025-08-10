use anyhow::Result;
use stoat_agent_claude_code::ClaudeCode;
use tokio::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("stoat_agent_claude_code=debug,info")),
        )
        .init();

    info!("Starting Claude Code example");

    // Create ClaudeCode instance using builder
    let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

    // Send initial message
    info!("Sending initial message...");
    claude.send_message("What is 2+2?").await?;

    // Wait for initial response
    if let Some(response) = claude.wait_for_response(Duration::from_secs(10)).await? {
        info!("Assistant response: {}", response);
    }

    // Send a follow-up message
    info!("Sending follow-up message...");
    claude
        .send_message("Can you also tell me what 10*10 is?")
        .await?;

    // Wait for response
    if let Some(response) = claude.wait_for_response(Duration::from_secs(10)).await? {
        info!("Assistant response: {}", response);
    }

    // Gracefully shutdown
    info!("Shutting down...");
    claude.shutdown().await?;

    Ok(())
}

// Example 2: Using with specific tools
#[allow(dead_code)]
async fn example_with_tools() -> Result<()> {
    let claude = ClaudeCode::builder()
        .allowed_tools(vec!["read_file".to_string(), "write_file".to_string()])
        .model("sonnet")
        .build()
        .await?;

    claude
        .send_message("Help me create a hello world file")
        .await?;

    // ... interact with claude ...

    claude.shutdown().await?;
    Ok(())
}

// Example 3: Resuming a session
#[allow(dead_code)]
async fn example_resume_session(session_id: String) -> Result<()> {
    let claude = ClaudeCode::builder()
        .session_id(session_id)
        .model("sonnet")
        .build()
        .await?;

    claude.send_message("Continue where we left off").await?;

    // ... interact with claude ...

    claude.shutdown().await?;
    Ok(())
}
