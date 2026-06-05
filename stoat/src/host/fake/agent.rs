#![allow(clippy::unwrap_used)]

//! In-memory fakes for the ACP host traits, so UI/app tests drive an
//! agent conversation without spawning a real process. Mirrors
//! [`super::claude_code`]'s `FakeClaudeCode`/`FakeClaudeCodeHost`.

use crate::host::{
    agent::{AgentConnection, AgentSession},
    claude_code::AgentMessage,
};
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

/// Fake [`AgentSession`]: `push_*` enqueues an [`AgentMessage`] that the
/// next `recv()` yields. `prompt`/`cancel` are recorded for assertions.
pub struct FakeAgentSession {
    tx: Mutex<Option<mpsc::UnboundedSender<AgentMessage>>>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<AgentMessage>>,
    prompts: Mutex<Vec<String>>,
    cancels: Mutex<u32>,
}

impl FakeAgentSession {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx: Mutex::new(Some(tx)),
            rx: tokio::sync::Mutex::new(rx),
            prompts: Mutex::new(Vec::new()),
            cancels: Mutex::new(0),
        }
    }

    /// A session pre-loaded to stream a single agent text message.
    pub fn single_turn(text: &str) -> Self {
        let session = Self::new();
        session.push_text(text);
        session
    }

    /// Enqueue an arbitrary [`AgentMessage`] for the next `recv()`.
    pub fn push(&self, message: AgentMessage) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(message);
        }
    }

    pub fn push_text(&self, text: &str) {
        self.push(AgentMessage::Text {
            text: text.to_string(),
        });
    }

    pub fn push_thinking(&self, text: &str) {
        self.push(AgentMessage::Thinking {
            text: text.to_string(),
            signature: String::new(),
        });
    }

    /// Prompts the session received, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    /// How many times `cancel` was called.
    pub fn cancel_count(&self) -> u32 {
        *self.cancels.lock().unwrap()
    }
}

impl Default for FakeAgentSession {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentSession for FakeAgentSession {
    async fn prompt(&self, content: &str) -> io::Result<()> {
        self.prompts.lock().unwrap().push(content.to_string());
        Ok(())
    }

    async fn cancel(&self) -> io::Result<()> {
        *self.cancels.lock().unwrap() += 1;
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

/// Fake [`AgentConnection`] that hands out pre-configured
/// [`FakeAgentSession`]s on each `new_session` call.
pub struct FakeAgentConnection {
    sessions: Mutex<VecDeque<Arc<FakeAgentSession>>>,
}

impl FakeAgentConnection {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(VecDeque::new()),
        }
    }

    /// Enqueue a session handed out on the next `new_session` call.
    /// Returns the shared reference so callers can keep pushing messages
    /// after the connection hands the session off.
    pub fn push_session(&self, session: FakeAgentSession) -> Arc<FakeAgentSession> {
        let arc = Arc::new(session);
        self.sessions.lock().unwrap().push_back(arc.clone());
        arc
    }
}

impl Default for FakeAgentConnection {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentConnection for FakeAgentConnection {
    async fn new_session(&self) -> io::Result<Box<dyn AgentSession>> {
        let arc = self
            .sessions
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("no fake agent sessions queued"))?;
        Ok(Box::new(ArcAgentSession(arc)))
    }
}

/// Bridge so the connection hands out a `Box<dyn AgentSession>` while a
/// test keeps its own `Arc<FakeAgentSession>`; both target the same
/// channels.
struct ArcAgentSession(Arc<FakeAgentSession>);

#[async_trait]
impl AgentSession for ArcAgentSession {
    async fn prompt(&self, content: &str) -> io::Result<()> {
        self.0.prompt(content).await
    }

    async fn cancel(&self) -> io::Result<()> {
        self.0.cancel().await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pushed_messages_arrive_in_order() {
        let session = FakeAgentSession::new();
        session.push_text("hello");
        session.push_thinking("hmm");

        match session.recv().await {
            Some(AgentMessage::Text { text }) => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
        match session.recv().await {
            Some(AgentMessage::Thinking { text, .. }) => assert_eq!(text, "hmm"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_and_cancel_are_recorded() {
        let session = FakeAgentSession::new();
        session.prompt("do it").await.unwrap();
        session.cancel().await.unwrap();
        assert_eq!(session.prompts(), vec!["do it".to_string()]);
        assert_eq!(session.cancel_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_ends_the_stream() {
        let session = FakeAgentSession::new();
        assert!(session.is_alive());
        session.shutdown().await.unwrap();
        assert!(!session.is_alive());
        assert!(session.recv().await.is_none());
    }

    #[tokio::test]
    async fn connection_hands_out_queued_sessions() {
        let conn = FakeAgentConnection::new();
        let handle = conn.push_session(FakeAgentSession::single_turn("from-queue"));

        let session = conn.new_session().await.unwrap();
        match session.recv().await {
            Some(AgentMessage::Text { text }) => assert_eq!(text, "from-queue"),
            other => panic!("expected Text, got {other:?}"),
        }
        // The handle and the handed-out session share channels.
        session.prompt("hi").await.unwrap();
        assert_eq!(handle.prompts(), vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn empty_queue_errors() {
        let conn = FakeAgentConnection::new();
        assert!(conn.new_session().await.is_err());
    }
}
