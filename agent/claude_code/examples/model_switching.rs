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

    info!("Starting model switching example");

    // Create ClaudeCode instance with initial model
    let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

    // Display session info
    info!("Session ID: {}", claude.get_session_id());

    // Send a message with the first model
    info!("Asking sonnet model a question...");
    claude
        .send_message("What is the capital of France? Reply in one word.")
        .await?;

    // Wait for response
    match claude.wait_for_response(Duration::from_secs(30)).await? {
        Some(response) => {
            info!("Sonnet response: {}", response);
        },
        None => {
            info!("No response received from sonnet");
        },
    }

    // Switch to a different model
    info!("Switching to haiku model...");
    claude.switch_model("haiku").await?;

    // Send another message with the new model
    info!("Asking haiku model a question...");
    claude
        .send_message("What is the capital of Japan? Reply in one word.")
        .await?;

    // Wait for response from new model
    match claude.wait_for_response(Duration::from_secs(30)).await? {
        Some(response) => {
            info!("Haiku response: {}", response);
        },
        None => {
            info!("No response received from haiku");
        },
    }

    // Demonstrate that conversation history is preserved
    info!("Asking about previous conversation...");
    claude
        .send_message("What were the two capitals I asked about?")
        .await?;

    match claude.wait_for_response(Duration::from_secs(30)).await? {
        Some(response) => {
            info!("Model remembers: {}", response);
        },
        None => {
            info!("No response received");
        },
    }

    // Gracefully shutdown
    info!("Shutting down...");
    claude.shutdown().await?;

    Ok(())
}
