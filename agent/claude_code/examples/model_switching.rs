use anyhow::Result;
use std::time::Duration;
use stoat_agent_claude_code::ClaudeCode;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("stoat_agent_claude_code=debug,info")),
        )
        .init();

    smol::block_on(async {
        info!("Starting model switching example");

        let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

        info!("Session ID: {}", claude.get_session_id());

        info!("Asking sonnet model a question...");
        claude
            .send_message("What is the capital of France? Reply in one word.")
            .await?;

        match claude.wait_for_response(Duration::from_secs(30)).await? {
            Some(response) => {
                info!("Sonnet response: {}", response);
            },
            None => {
                info!("No response received from sonnet");
            },
        }

        info!("Switching to haiku model...");
        claude.switch_model("haiku").await?;

        info!("Asking haiku model a question...");
        claude
            .send_message("What is the capital of Japan? Reply in one word.")
            .await?;

        match claude.wait_for_response(Duration::from_secs(30)).await? {
            Some(response) => {
                info!("Haiku response: {}", response);
            },
            None => {
                info!("No response received from haiku");
            },
        }

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

        info!("Shutting down...");
        claude.shutdown()?;

        Ok(())
    })
}
