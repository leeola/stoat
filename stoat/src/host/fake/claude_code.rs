use crate::host::claude_code::{AgentMessage, ClaudeCodeHost};
use async_trait::async_trait;
use std::{collections::VecDeque, io, sync::Mutex};

pub struct FakeClaudeCode {
    state: Mutex<FakeClaudeCodeState>,
}

struct FakeClaudeCodeState {
    outgoing: VecDeque<AgentMessage>,
    sent: Vec<String>,
    shut_down: bool,
}

impl Default for FakeClaudeCode {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClaudeCode {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeClaudeCodeState {
                outgoing: VecDeque::new(),
                sent: Vec::new(),
                shut_down: false,
            }),
        }
    }

    // --- Push individual messages ---

    pub fn push_init(&self) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::Init {
                session_id: "test-session".to_string(),
                model: "test-model".to_string(),
                tools: vec!["Read".to_string(), "Write".to_string(), "Bash".to_string()],
            });
    }

    pub fn push_text(&self, text: &str) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::Text {
                text: text.to_string(),
            });
    }

    pub fn push_tool_use(&self, name: &str, input: &str) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::ToolUse {
                id: format!("toolu_{name}"),
                name: name.to_string(),
                input: input.to_string(),
            });
    }

    pub fn push_tool_result(&self, id: &str, content: &str) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::ToolResult {
                id: id.to_string(),
                content: content.to_string(),
            });
    }

    pub fn push_thinking(&self, text: &str) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::Thinking {
                text: text.to_string(),
                signature: "test-sig".to_string(),
            });
    }

    pub fn push_result(&self) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::Result {
                cost_usd: 0.001,
                duration_ms: 500,
                num_turns: 1,
            });
    }

    pub fn push_error(&self, message: &str) {
        self.state
            .lock()
            .unwrap()
            .outgoing
            .push_back(AgentMessage::Error {
                message: message.to_string(),
            });
    }

    // --- Scenario builders ---

    pub fn single_turn(response: &str) -> Self {
        let fake = Self::new();
        fake.push_init();
        fake.push_text(response);
        fake.push_result();
        fake
    }

    pub fn tool_turn(tool: &str, input: &str, response: &str) -> Self {
        let fake = Self::new();
        fake.push_init();
        fake.push_tool_use(tool, input);
        fake.push_tool_result(&format!("toolu_{tool}"), "success");
        fake.push_text(response);
        fake.push_result();
        fake
    }

    pub fn multi_turn(responses: &[&str]) -> Self {
        let fake = Self::new();
        fake.push_init();
        for response in responses {
            fake.push_text(response);
        }
        fake.push_result();
        fake
    }

    // --- Assertion helpers ---

    pub fn sent_messages(&self) -> Vec<String> {
        self.state.lock().unwrap().sent.clone()
    }

    pub fn assert_sent(&self, index: usize, expected: &str) {
        let state = self.state.lock().unwrap();
        assert_eq!(
            state.sent.get(index).map(String::as_str),
            Some(expected),
            "sent[{index}] mismatch"
        );
    }

    pub fn assert_send_count(&self, count: usize) {
        let state = self.state.lock().unwrap();
        assert_eq!(state.sent.len(), count, "send count mismatch");
    }
}

#[async_trait]
impl ClaudeCodeHost for FakeClaudeCode {
    async fn send(&self, content: &str) -> io::Result<()> {
        self.state.lock().unwrap().sent.push(content.to_string());
        Ok(())
    }

    async fn recv(&self) -> Option<AgentMessage> {
        self.state.lock().unwrap().outgoing.pop_front()
    }

    fn is_alive(&self) -> bool {
        !self.state.lock().unwrap().shut_down
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.state.lock().unwrap().shut_down = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn single_turn_scenario() {
        rt().block_on(async {
            let agent = FakeClaudeCode::single_turn("The answer is 42.");

            let init = agent.recv().await.expect("should have init");
            assert!(matches!(init, AgentMessage::Init { .. }));

            agent.send("What is the meaning of life?").await.unwrap();

            let response = agent.recv().await.expect("should have text");
            match response {
                AgentMessage::Text { text } => assert_eq!(text, "The answer is 42."),
                other => panic!("expected Text, got {other:?}"),
            }

            let result = agent.recv().await.expect("should have result");
            assert!(matches!(result, AgentMessage::Result { .. }));

            assert!(agent.recv().await.is_none());
            agent.assert_send_count(1);
            agent.assert_sent(0, "What is the meaning of life?");
        });
    }

    #[test]
    fn tool_turn_scenario() {
        rt().block_on(async {
            let agent = FakeClaudeCode::tool_turn("Read", r#"{"path":"/tmp/f.txt"}"#, "Done.");

            let _ = agent.recv().await;
            let tool = agent.recv().await.expect("should have tool_use");
            match tool {
                AgentMessage::ToolUse { name, input, .. } => {
                    assert_eq!(name, "Read");
                    assert!(input.contains("/tmp/f.txt"));
                },
                other => panic!("expected ToolUse, got {other:?}"),
            }

            let result = agent.recv().await.expect("should have tool_result");
            assert!(matches!(result, AgentMessage::ToolResult { .. }));

            let text = agent.recv().await.expect("should have text");
            assert!(matches!(text, AgentMessage::Text { text } if text == "Done."));
        });
    }

    #[test]
    fn multi_turn_scenario() {
        rt().block_on(async {
            let agent = FakeClaudeCode::multi_turn(&["First.", "Second.", "Third."]);

            let _ = agent.recv().await; // init
            for expected in ["First.", "Second.", "Third."] {
                let msg = agent.recv().await.expect("should have text");
                match msg {
                    AgentMessage::Text { text } => assert_eq!(text, expected),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            assert!(matches!(
                agent.recv().await,
                Some(AgentMessage::Result { .. })
            ));
        });
    }

    #[test]
    fn push_individual_messages() {
        rt().block_on(async {
            let agent = FakeClaudeCode::new();
            agent.push_init();
            agent.push_error("something went wrong");

            let init = agent.recv().await.unwrap();
            assert!(matches!(init, AgentMessage::Init { .. }));

            let error = agent.recv().await.unwrap();
            match error {
                AgentMessage::Error { message } => assert_eq!(message, "something went wrong"),
                other => panic!("expected Error, got {other:?}"),
            }
        });
    }

    #[test]
    fn is_alive_until_shutdown() {
        rt().block_on(async {
            let agent = FakeClaudeCode::new();
            assert!(agent.is_alive());
            agent.shutdown().await.unwrap();
            assert!(!agent.is_alive());
        });
    }

    #[test]
    fn sent_messages_captured() {
        rt().block_on(async {
            let agent = FakeClaudeCode::new();
            agent.send("hello").await.unwrap();
            agent.send("world").await.unwrap();

            let sent = agent.sent_messages();
            assert_eq!(sent, ["hello", "world"]);
        });
    }
}
