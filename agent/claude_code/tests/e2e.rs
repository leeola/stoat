#[cfg(feature = "e2e_claude_code")]
mod e2e_tests {
    use stoat_agent_claude_code::ClaudeCode;
    use tokio::time::Duration;
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

    #[tokio::test]
    async fn test_basic_math_query() {
        init_logging();

        info!("Starting e2e test");

        // Create ClaudeCode instance with builder
        info!("Creating ClaudeCode instance");
        let claude = ClaudeCode::builder()
            .model("sonnet")
            .build()
            .await
            .expect("Failed to spawn Claude");

        info!("ClaudeCode instance created successfully");

        // Create mutable ClaudeCode for wait_for_response
        let mut claude = claude;

        // Send the initial message
        info!("Sending initial message");
        claude
            .send_message("What is 2+2? Reply with just the number.")
            .await
            .expect("Failed to send message");
        info!("Message sent successfully");

        // Wait for response with timeout
        info!("Waiting for assistant response (30s timeout)");
        let response = claude
            .wait_for_response(Duration::from_secs(30))
            .await
            .expect("Failed to wait for response");

        // Verify we got an assistant message with the expected content
        let content = response.expect("No assistant message received");
        assert!(
            content.contains("4"),
            "Expected response to contain '4', got: {content}"
        );

        // Clean shutdown
        info!("Shutting down");
        claude.shutdown().await.expect("Failed to shutdown");
        info!("Test completed successfully");
    }

    #[tokio::test]
    async fn test_wait_for_response() {
        init_logging();

        info!("Starting wait_for_response test");

        // Create ClaudeCode instance
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

        // Wait for response using the helper
        info!("Waiting for response");
        let response = claude
            .wait_for_response(Duration::from_secs(30))
            .await
            .expect("Failed to wait for response");

        // Verify we got a response
        let content = response.expect("Expected to receive a response");
        assert!(
            content.contains("20"),
            "Expected response to contain '20', got: {content}"
        );

        // Test is_alive
        assert!(claude.is_alive().await, "Expected Claude to be alive");

        // Clean shutdown
        info!("Shutting down");
        claude.shutdown().await.expect("Failed to shutdown");
        info!("Test completed successfully");
    }

    #[tokio::test]
    async fn test_model_switching() {
        init_logging();

        info!("Starting model switching test");

        // Create ClaudeCode instance
        let mut claude = ClaudeCode::builder()
            .model("sonnet")
            .build()
            .await
            .expect("Failed to spawn Claude");

        // Get initial session ID
        let session_id = claude.get_session_id().to_string();
        info!("Initial session ID: {}", session_id);

        // Send a message with first model
        info!("Sending message with first model");
        claude
            .send_message("What is 1+1? Reply with just the number.")
            .await
            .expect("Failed to send message");

        // Wait for response
        let response1 = claude
            .wait_for_response(Duration::from_secs(30))
            .await
            .expect("Failed to wait for response")
            .expect("Expected to receive a response");

        assert!(
            response1.contains("2"),
            "Expected response to contain '2', got: {response1}"
        );

        // Switch to a different model
        info!("Switching to haiku model");
        claude
            .switch_model("haiku")
            .await
            .expect("Failed to switch model");

        // Verify session ID is preserved
        assert_eq!(
            claude.get_session_id(),
            session_id,
            "Session ID should be preserved after model switch"
        );

        // Send another message with new model
        info!("Sending message with new model");
        claude
            .send_message("What is 3+3? Reply with just the number.")
            .await
            .expect("Failed to send message");

        // Wait for response from new model
        let response2 = claude
            .wait_for_response(Duration::from_secs(30))
            .await
            .expect("Failed to wait for response")
            .expect("Expected to receive a response");

        assert!(
            response2.contains("6"),
            "Expected response to contain '6', got: {response2}"
        );

        // Verify process is still alive
        assert!(
            claude.is_alive().await,
            "Expected Claude to be alive after switch"
        );

        // Clean shutdown
        info!("Shutting down");
        claude.shutdown().await.expect("Failed to shutdown");
        info!("Model switching test completed successfully");
    }
}
