//! ACP (Agent Client Protocol) agent host: the message schema and the
//! initialize/session lifecycle, wired into the
//! [`AgentConnection`]/[`AgentSession`] host traits over the reusable
//! JSON-RPC transport.
//!
//! [`AcpConnection::connect`] runs the `initialize` handshake advertising
//! client capabilities, then spawns [`AcpSession`] handles that send
//! `session/prompt` and `session/cancel`. The streamed `session/update`
//! demux that drives [`AgentSession::recv`] is implemented separately;
//! until then `recv` yields nothing.

mod schema;

use crate::schema::{
    CancelParams, ClientCapabilities, ContentBlock, FileSystemCapabilities, InitializeParams,
    NewSessionParams, NewSessionResult, PromptParams, INITIALIZE, PROTOCOL_VERSION, SESSION_CANCEL,
    SESSION_NEW, SESSION_PROMPT,
};
use async_trait::async_trait;
use snafu::{Location, ResultExt, Snafu};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat::host::{AgentConnection, AgentMessage, AgentSession};
use stoat_agent_claude_code::jsonrpc::{JsonRpcPeer, TransportError};
use stoat_scheduler::{Executor, Task};

/// Failure modes of [`AcpConnection::connect`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum AcpError {
    #[snafu(display("failed to encode an ACP request"))]
    Encode {
        source: serde_json::Error,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("ACP transport request failed"))]
    Transport {
        source: TransportError,
        #[snafu(implicit)]
        location: Location,
    },
}

/// An established ACP connection: a session manager over the JSON-RPC
/// transport. Construct with [`Self::connect`]; spawn conversations with
/// [`AgentConnection::new_session`].
pub struct AcpConnection {
    peer: Arc<JsonRpcPeer>,
    executor: Executor,
    cwd: String,
}

impl AcpConnection {
    /// Run the ACP `initialize` handshake over an established transport,
    /// advertising filesystem read/write and terminal client
    /// capabilities, and return a ready connection. New sessions open in
    /// `cwd`.
    pub async fn connect(
        peer: JsonRpcPeer,
        executor: Executor,
        cwd: impl Into<String>,
    ) -> Result<Self, AcpError> {
        let peer = Arc::new(peer);
        let params = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                },
                terminal: true,
            },
        })
        .context(EncodeSnafu)?;
        peer.request(INITIALIZE, Some(params))
            .await
            .context(TransportSnafu)?;
        Ok(Self {
            peer,
            executor,
            cwd: cwd.into(),
        })
    }
}

#[async_trait]
impl AgentConnection for AcpConnection {
    async fn new_session(&self) -> io::Result<Box<dyn AgentSession>> {
        let params = serde_json::to_value(NewSessionParams {
            cwd: self.cwd.clone(),
        })
        .map_err(io::Error::other)?;
        let result = self
            .peer
            .request(SESSION_NEW, Some(params))
            .await
            .map_err(io::Error::other)?;
        let session: NewSessionResult = serde_json::from_value(result).map_err(io::Error::other)?;
        Ok(Box::new(AcpSession::new(
            Arc::clone(&self.peer),
            self.executor.clone(),
            session.session_id,
        )))
    }
}

/// One ACP conversation. Sends `session/prompt` and `session/cancel`
/// over the shared transport; the streamed agent output is delivered by
/// the (separate) `session/update` demux.
struct AcpSession {
    peer: Arc<JsonRpcPeer>,
    executor: Executor,
    session_id: String,
    alive: AtomicBool,
    /// The in-flight `session/prompt` request task. Held so the request
    /// frame is actually sent -- dropping a [`Task`] cancels it -- and
    /// replaced when the next prompt starts.
    turn: Mutex<Option<Task<()>>>,
}

impl AcpSession {
    fn new(peer: Arc<JsonRpcPeer>, executor: Executor, session_id: String) -> Self {
        Self {
            peer,
            executor,
            session_id,
            alive: AtomicBool::new(true),
            turn: Mutex::new(None),
        }
    }
}

#[async_trait]
impl AgentSession for AcpSession {
    async fn prompt(&self, content: &str) -> io::Result<()> {
        let params = serde_json::to_value(PromptParams {
            session_id: self.session_id.clone(),
            prompt: vec![ContentBlock::text(content)],
        })
        .map_err(io::Error::other)?;
        let peer = Arc::clone(&self.peer);
        let task = self.executor.spawn(async move {
            // FIXME: route the turn-end stop reason into `recv` once the
            // session/update demux lands.
            let _ = peer.request(SESSION_PROMPT, Some(params)).await;
        });
        *self.turn.lock().expect("turn mutex") = Some(task);
        Ok(())
    }

    async fn cancel(&self) -> io::Result<()> {
        let params = serde_json::to_value(CancelParams {
            session_id: self.session_id.clone(),
        })
        .map_err(io::Error::other)?;
        self.peer
            .notify(SESSION_CANCEL, Some(params))
            .map_err(io::Error::other)
    }

    async fn recv(&self) -> Option<AgentMessage> {
        // FIXME: demux the session/update notification stream into
        // AgentMessage once that item lands; until then the session
        // streams no events.
        None
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    async fn shutdown(&self) -> io::Result<()> {
        self.alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use stoat_agent_claude_code::jsonrpc::Incoming;
    use stoat_scheduler::TokioScheduler;
    use tokio::sync::mpsc;

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    /// Drive the agent end of a duplex: answer
    /// initialize/session-new/session-prompt requests with canned results
    /// and forward every received (method, params) to `seen` so a test can
    /// assert what the client sent.
    fn fake_agent(
        executor: &Executor,
        incoming: Incoming,
        seen: mpsc::UnboundedSender<(String, Value)>,
    ) -> Task<()> {
        let Incoming {
            mut requests,
            mut notifications,
        } = incoming;
        executor.spawn(async move {
            loop {
                tokio::select! {
                    Some(req) = requests.recv() => {
                        let _ = seen.send((req.method.clone(), req.params.clone().unwrap_or(Value::Null)));
                        let response = match req.method.as_str() {
                            "initialize" => json!({ "protocolVersion": 1 }),
                            "session/new" => json!({ "sessionId": "sess-1" }),
                            "session/prompt" => json!({ "stopReason": "endTurn" }),
                            _ => json!({}),
                        };
                        let _ = req.respond(Ok(response));
                    }
                    Some(notif) = notifications.recv() => {
                        let _ = seen.send((notif.method.clone(), notif.params.clone().unwrap_or(Value::Null)));
                    }
                    else => break,
                }
            }
        })
    }

    /// Connect against a fresh fake agent, draining the `initialize` frame.
    /// Returns the connection, the `seen` receiver, and the agent/server
    /// task handles (held to keep the duplex alive).
    async fn connected(
        executor: &Executor,
    ) -> (
        AcpConnection,
        mpsc::UnboundedReceiver<(String, Value)>,
        (Task<()>, JsonRpcPeer, Incoming),
    ) {
        let ((client, client_in), (server, server_in)) = JsonRpcPeer::duplex(executor);
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
        let agent = fake_agent(executor, server_in, seen_tx);
        let conn = AcpConnection::connect(client, executor.clone(), "/work")
            .await
            .expect("connect");
        let init = seen_rx.recv().await.expect("initialize seen");
        assert_eq!(init.0, "initialize");
        (conn, seen_rx, (agent, server, client_in))
    }

    #[tokio::test]
    async fn connect_runs_initialize_with_client_capabilities() {
        let executor = executor();
        let ((client, _client_in), (_server, server_in)) = JsonRpcPeer::duplex(&executor);
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
        let _agent = fake_agent(&executor, server_in, seen_tx);

        let _conn = AcpConnection::connect(client, executor.clone(), "/work")
            .await
            .expect("connect");

        let (method, params) = seen_rx.recv().await.expect("initialize seen");
        assert_eq!(method, "initialize");
        assert_eq!(params["protocolVersion"], json!(1));
        let caps = &params["clientCapabilities"];
        assert_eq!(caps["fs"]["readTextFile"], json!(true));
        assert_eq!(caps["fs"]["writeTextFile"], json!(true));
        assert_eq!(caps["terminal"], json!(true));
    }

    #[tokio::test]
    async fn new_session_sends_cwd_and_returns_live_session() {
        let executor = executor();
        let (conn, mut seen_rx, _keep) = connected(&executor).await;

        let session = conn.new_session().await.expect("new_session");
        assert!(session.is_alive());

        let (method, params) = seen_rx.recv().await.expect("session/new seen");
        assert_eq!(method, "session/new");
        assert_eq!(params["cwd"], json!("/work"));
    }

    #[tokio::test]
    async fn prompt_sends_session_prompt_with_text_block() {
        let executor = executor();
        let (conn, mut seen_rx, _keep) = connected(&executor).await;
        let session = conn.new_session().await.expect("new_session");
        let _ = seen_rx.recv().await.expect("session/new seen");

        session.prompt("hello").await.expect("prompt");

        let (method, params) = seen_rx.recv().await.expect("session/prompt seen");
        assert_eq!(method, "session/prompt");
        assert_eq!(params["sessionId"], json!("sess-1"));
        assert_eq!(
            params["prompt"],
            json!([{ "type": "text", "text": "hello" }])
        );
    }

    #[tokio::test]
    async fn cancel_sends_session_cancel_notification() {
        let executor = executor();
        let (conn, mut seen_rx, _keep) = connected(&executor).await;
        let session = conn.new_session().await.expect("new_session");
        let _ = seen_rx.recv().await.expect("session/new seen");

        session.cancel().await.expect("cancel");

        let (method, params) = seen_rx.recv().await.expect("session/cancel seen");
        assert_eq!(method, "session/cancel");
        assert_eq!(params["sessionId"], json!("sess-1"));
    }
}
