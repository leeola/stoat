use crate::host::claude_code::{
    AgentMessage, ClaudeCodeHost, ClaudeCodeSession, ClaudeSessionSummary, HookLifecycleEvent,
    PlanEntry, SessionStateEvent, TaskEvent, TokenUsage, ToolCallStatus, ToolKind,
};
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

/// In-memory fake implementing [`ClaudeCodeSession`]. `push_*` methods
/// enqueue outbound messages that will be delivered to the polling task
/// the next time `recv()` is polled. `recv()` awaits until a message is
/// available or `shutdown` is called.
pub struct FakeClaudeCode {
    tx: Mutex<Option<mpsc::UnboundedSender<AgentMessage>>>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<AgentMessage>>,
    sent: Mutex<Vec<String>>,
}

impl Default for FakeClaudeCode {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClaudeCode {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx: Mutex::new(Some(tx)),
            rx: tokio::sync::Mutex::new(rx),
            sent: Mutex::new(Vec::new()),
        }
    }

    fn enqueue(&self, msg: AgentMessage) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }

    // --- Push individual messages ---

    pub fn push_init(&self) {
        self.enqueue(AgentMessage::Init {
            session_id: "test-session".to_string(),
            model: "test-model".to_string(),
            tools: vec!["Read".to_string(), "Write".to_string(), "Bash".to_string()],
        });
    }

    pub fn push_text(&self, text: &str) {
        self.enqueue(AgentMessage::Text {
            text: text.to_string(),
        });
    }

    pub fn push_partial_text(&self, text: &str) {
        self.enqueue(AgentMessage::PartialText {
            text: text.to_string(),
        });
    }

    pub fn push_tool_use(&self, name: &str, input: &str) {
        self.enqueue(AgentMessage::ToolUse {
            id: format!("toolu_{name}"),
            name: name.to_string(),
            input: input.to_string(),
            kind: ToolKind::Other,
            title: name.to_string(),
            content: Vec::new(),
            locations: Vec::new(),
        });
    }

    pub fn push_tool_result(&self, id: &str, content: &str) {
        self.enqueue(AgentMessage::ToolResult {
            id: id.to_string(),
            content: content.to_string(),
            status: ToolCallStatus::Completed,
            kind: ToolKind::Other,
            terminal_meta: None,
        });
    }

    pub fn push_tool_update(&self, id: &str, status: ToolCallStatus) {
        self.enqueue(AgentMessage::ToolUpdate {
            id: id.to_string(),
            content: Vec::new(),
            status,
        });
    }

    pub fn push_partial_tool_input(&self, id: &str, delta: &str) {
        self.enqueue(AgentMessage::PartialToolInput {
            id: id.to_string(),
            json_delta: delta.to_string(),
        });
    }

    pub fn push_plan(&self, entries: Vec<PlanEntry>) {
        self.enqueue(AgentMessage::Plan { entries });
    }

    pub fn push_usage(&self, accumulated: TokenUsage, last: TokenUsage) {
        self.enqueue(AgentMessage::Usage { accumulated, last });
    }

    pub fn push_mode_changed(&self, mode: &str) {
        self.enqueue(AgentMessage::ModeChanged {
            mode: mode.to_string(),
        });
    }

    pub fn push_model_changed(&self, model: &str) {
        self.enqueue(AgentMessage::ModelChanged {
            model: model.to_string(),
        });
    }

    pub fn push_files_persisted(&self, paths: Vec<PathBuf>) {
        self.enqueue(AgentMessage::FilesPersisted { paths });
    }

    pub fn push_elicitation_complete(&self, id: &str, outcome_json: &str) {
        self.enqueue(AgentMessage::ElicitationComplete {
            id: id.to_string(),
            outcome_json: outcome_json.to_string(),
        });
    }

    pub fn push_auth_required(&self, reason: &str) {
        self.enqueue(AgentMessage::AuthRequired {
            reason: reason.to_string(),
        });
    }

    pub fn push_session_state(&self, event: SessionStateEvent) {
        self.enqueue(AgentMessage::SessionState(event));
    }

    pub fn push_task_event(&self, event: TaskEvent) {
        self.enqueue(AgentMessage::TaskEvent(event));
    }

    pub fn push_hook(&self, event: HookLifecycleEvent) {
        self.enqueue(AgentMessage::Hook(event));
    }

    pub fn push_thinking(&self, text: &str) {
        self.enqueue(AgentMessage::Thinking {
            text: text.to_string(),
            signature: "test-sig".to_string(),
        });
    }

    pub fn push_result(&self) {
        self.enqueue(AgentMessage::Result {
            cost_usd: 0.001,
            duration_ms: 500,
            num_turns: 1,
        });
    }

    pub fn push_result_with(&self, cost_usd: f64, duration_ms: u64, num_turns: u32) {
        self.enqueue(AgentMessage::Result {
            cost_usd,
            duration_ms,
            num_turns,
        });
    }

    pub fn push_error(&self, message: &str) {
        self.enqueue(AgentMessage::Error {
            message: message.to_string(),
        });
    }

    /// Enqueue a fully-formed [`AgentMessage`]. Escape hatch for callers that
    /// want control over every field (e.g. tests that need a specific
    /// `ToolKind` classification). Prefer the typed `push_*` helpers.
    pub fn push_raw(&self, msg: AgentMessage) {
        self.enqueue(msg);
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
        self.sent.lock().unwrap().clone()
    }

    pub fn assert_sent(&self, index: usize, expected: &str) {
        let sent = self.sent.lock().unwrap();
        assert_eq!(
            sent.get(index).map(String::as_str),
            Some(expected),
            "sent[{index}] mismatch"
        );
    }

    pub fn assert_send_count(&self, count: usize) {
        let sent = self.sent.lock().unwrap();
        assert_eq!(sent.len(), count, "send count mismatch");
    }
}

#[async_trait]
impl ClaudeCodeSession for FakeClaudeCode {
    async fn send(&self, content: &str) -> io::Result<()> {
        self.sent.lock().unwrap().push(content.to_string());
        Ok(())
    }

    async fn recv(&self) -> Option<AgentMessage> {
        self.rx.lock().await.recv().await
    }

    fn is_alive(&self) -> bool {
        self.tx.lock().unwrap().is_some()
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.tx.lock().unwrap().take();
        Ok(())
    }
}

pub struct FakeClaudeCodeHost {
    sessions: Mutex<VecDeque<Arc<FakeClaudeCode>>>,
    summaries: Mutex<Vec<ClaudeSessionSummary>>,
}

impl Default for FakeClaudeCodeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClaudeCodeHost {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(VecDeque::new()),
            summaries: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue a fake session to be handed out on the next [`new_session`]
    /// call. Returns the shared reference so callers can continue to push
    /// messages after the host has handed the session off.
    pub fn push_session(&self, session: FakeClaudeCode) -> Arc<FakeClaudeCode> {
        let arc = Arc::new(session);
        self.sessions.lock().unwrap().push_back(arc.clone());
        arc
    }

    /// Register a session summary to be returned by
    /// [`ClaudeCodeHost::list_sessions`]. Does not seed any actual session;
    /// callers are responsible for pairing summaries with sessions when
    /// they want the two to describe the same entity.
    pub fn register_summary(&self, summary: ClaudeSessionSummary) {
        self.summaries.lock().unwrap().push(summary);
    }
}

/// Trait-object bridge so the host can hand out a
/// `Box<dyn ClaudeCodeSession>` while the test harness retains its own
/// `Arc<FakeClaudeCode>` reference. Both paths target the same underlying
/// channels.
pub(crate) struct ArcSession(pub(crate) Arc<FakeClaudeCode>);

#[async_trait]
impl ClaudeCodeSession for ArcSession {
    async fn send(&self, content: &str) -> io::Result<()> {
        self.0.send(content).await
    }

    async fn recv(&self) -> Option<AgentMessage> {
        self.0.recv().await
    }

    fn is_alive(&self) -> bool {
        self.0.is_alive()
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.0.shutdown().await
    }
}

#[async_trait]
impl ClaudeCodeHost for FakeClaudeCodeHost {
    async fn new_session(&self) -> io::Result<Box<dyn ClaudeCodeSession>> {
        let arc = self
            .sessions
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("no fake sessions queued"))?;
        Ok(Box::new(ArcSession(arc)))
    }

    async fn list_sessions(&self) -> io::Result<Vec<ClaudeSessionSummary>> {
        Ok(self.summaries.lock().unwrap().clone())
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

            agent.shutdown().await.unwrap();
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

    #[test]
    fn recv_awaits_until_pushed() {
        rt().block_on(async {
            let agent = std::sync::Arc::new(FakeClaudeCode::new());
            let agent_for_push = agent.clone();

            let push_task = tokio::spawn(async move {
                tokio::task::yield_now().await;
                agent_for_push.push_text("delivered");
            });

            let msg = agent.recv().await.expect("should receive after push");
            match msg {
                AgentMessage::Text { text } => assert_eq!(text, "delivered"),
                other => panic!("expected Text, got {other:?}"),
            }
            push_task.await.unwrap();
        });
    }
}
