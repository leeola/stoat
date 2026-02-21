#[cfg(feature = "e2e_claude_code")]
mod e2e_tests {
    use std::time::Duration;
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

    #[test]
    fn test_basic_math_query() {
        init_logging();
        smol::block_on(async {
            info!("Starting e2e test");

            info!("Creating ClaudeCode instance");
            let mut claude = ClaudeCode::builder()
                .model("sonnet")
                .build()
                .await
                .expect("Failed to spawn Claude");

            info!("ClaudeCode instance created successfully");

            info!("Sending initial message");
            claude
                .send_message("What is 2+2? Reply with just the number.")
                .await
                .expect("Failed to send message");
            info!("Message sent successfully");

            info!("Waiting for assistant response (30s timeout)");
            let response = claude
                .wait_for_response(Duration::from_secs(30))
                .await
                .expect("Failed to wait for response");

            let content = response.expect("No assistant message received");
            assert!(
                content.contains("4"),
                "Expected response to contain '4', got: {content}"
            );

            info!("Shutting down");
            claude.shutdown().expect("Failed to shutdown");
            info!("Test completed successfully");
        });
    }

    #[test]
    fn test_wait_for_response() {
        init_logging();
        smol::block_on(async {
            info!("Starting wait_for_response test");

            let mut claude = ClaudeCode::builder()
                .model("sonnet")
                .build()
                .await
                .expect("Failed to spawn Claude");

            info!("Sending message");
            claude
                .send_message("What is 10+10? Reply with just the number.")
                .await
                .expect("Failed to send message");

            info!("Waiting for response");
            let response = claude
                .wait_for_response(Duration::from_secs(30))
                .await
                .expect("Failed to wait for response");

            let content = response.expect("Expected to receive a response");
            assert!(
                content.contains("20"),
                "Expected response to contain '20', got: {content}"
            );

            assert!(claude.is_alive(), "Expected Claude to be alive");

            info!("Shutting down");
            claude.shutdown().expect("Failed to shutdown");
            info!("Test completed successfully");
        });
    }

    #[test]
    fn test_model_switching() {
        init_logging();
        smol::block_on(async {
            info!("Starting model switching test");

            let mut claude = ClaudeCode::builder()
                .model("sonnet")
                .build()
                .await
                .expect("Failed to spawn Claude");

            let session_id = claude.get_session_id().to_string();
            info!("Initial session ID: {}", session_id);

            info!("Sending message with first model");
            claude
                .send_message("What is 1+1? Reply with just the number.")
                .await
                .expect("Failed to send message");

            let response1 = claude
                .wait_for_response(Duration::from_secs(30))
                .await
                .expect("Failed to wait for response")
                .expect("Expected to receive a response");

            assert!(
                response1.contains("2"),
                "Expected response to contain '2', got: {response1}"
            );

            info!("Switching to haiku model");
            claude
                .switch_model("haiku")
                .await
                .expect("Failed to switch model");

            assert_eq!(
                claude.get_session_id(),
                session_id,
                "Session ID should be preserved after model switch"
            );

            info!("Sending message with new model");
            claude
                .send_message("What is 3+3? Reply with just the number.")
                .await
                .expect("Failed to send message");

            let response2 = claude
                .wait_for_response(Duration::from_secs(30))
                .await
                .expect("Failed to wait for response")
                .expect("Expected to receive a response");

            assert!(
                response2.contains("6"),
                "Expected response to contain '6', got: {response2}"
            );

            assert!(
                claude.is_alive(),
                "Expected Claude to be alive after switch"
            );

            info!("Shutting down");
            claude.shutdown().expect("Failed to shutdown");
            info!("Model switching test completed successfully");
        });
    }
}
