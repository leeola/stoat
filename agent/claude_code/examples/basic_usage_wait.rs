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
        let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

        claude
            .send_message("What is the capital of France?")
            .await?;

        match claude.wait_for_response(Duration::from_secs(30)).await? {
            Some(response) => {
                info!("Got response: {}", response);
            },
            None => {
                info!("No response received within timeout");
            },
        }

        if claude.is_alive() {
            info!("Claude is still running");
        }

        info!("Session ID: {}", claude.get_session_id());

        claude.shutdown()?;
        Ok(())
    })
}
