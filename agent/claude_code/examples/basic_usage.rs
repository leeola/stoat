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
        info!("Starting Claude Code example");

        let mut claude = ClaudeCode::builder().model("sonnet").build().await?;

        info!("Sending initial message...");
        claude.send_message("What is 2+2?").await?;

        if let Some(response) = claude.wait_for_response(Duration::from_secs(10)).await? {
            info!("Assistant response: {}", response);
        }

        info!("Sending follow-up message...");
        claude
            .send_message("Can you also tell me what 10*10 is?")
            .await?;

        if let Some(response) = claude.wait_for_response(Duration::from_secs(10)).await? {
            info!("Assistant response: {}", response);
        }

        info!("Shutting down...");
        claude.shutdown()?;

        Ok(())
    })
}

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

    claude.shutdown()?;
    Ok(())
}

#[allow(dead_code)]
async fn example_resume_session(session_id: String) -> Result<()> {
    let claude = ClaudeCode::builder()
        .session_id(session_id)
        .model("sonnet")
        .build()
        .await?;

    claude.send_message("Continue where we left off").await?;

    claude.shutdown()?;
    Ok(())
}
