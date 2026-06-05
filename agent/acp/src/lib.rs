//! ACP (Agent Client Protocol) agent host: the message schema and the
//! initialize/session lifecycle, wired into the
//! [`AgentConnection`]/[`AgentSession`] host traits over the reusable
//! JSON-RPC transport.
//!
//! [`AcpConnection::connect`] runs the `initialize` handshake advertising
//! client capabilities, then spawns [`AcpSession`] handles that send
//! `session/prompt` and `session/cancel`. A router task demuxes the
//! streamed `session/update` notifications into [`AgentMessage`] and
//! routes them by session to the matching [`AgentSession::recv`]; a
//! second task answers the agent's `fs/read_text_file` and
//! `fs/write_text_file` requests through the injected [`FsHost`].

mod demux;
mod fs;
mod permission;
mod rpc;
mod schema;

use crate::{
    demux::{demux_session_update, SessionUpdateNotification, SESSION_UPDATE},
    fs::handle_fs_request,
    permission::{handle_permission_request, SESSION_REQUEST_PERMISSION},
    rpc::method_not_found,
    schema::{
        CancelParams, ClientCapabilities, ContentBlock, FileSystemCapabilities, InitializeParams,
        NewSessionParams, NewSessionResult, PromptParams, INITIALIZE, PROTOCOL_VERSION,
        SESSION_CANCEL, SESSION_NEW, SESSION_PROMPT,
    },
};
use async_trait::async_trait;
use snafu::{Location, ResultExt, Snafu};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat::host::{AgentConnection, AgentMessage, AgentSession, FsHost, PermissionPrompt};
use stoat_agent_claude_code::jsonrpc::{
    Incoming, IncomingNotification, IncomingRequest, JsonRpcPeer, TransportError,
};
use stoat_scheduler::{Executor, Task};
use tokio::sync::{mpsc, Mutex as AsyncMutex};

/// Per-session [`AgentMessage`] senders the router routes demuxed
/// `session/update` events to, keyed by ACP session id.
type Routes = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<AgentMessage>>>>;

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
    routes: Routes,
    /// The task demuxing inbound `session/update` notifications to the
    /// per-session channels; held so it stays scheduled.
    _router: Task<()>,
    /// The task answering inbound `fs/*` requests through the injected
    /// [`FsHost`]; held so it stays scheduled.
    _request_router: Task<()>,
}

impl AcpConnection {
    /// Run the ACP `initialize` handshake over an established transport,
    /// advertising filesystem read/write and terminal client
    /// capabilities, and return a ready connection. New sessions open in
    /// `cwd`. `incoming` carries the agent's inbound frames: its
    /// `session/update` notifications are demuxed and routed to sessions,
    /// its `fs/*` requests are answered through `fs`, and each
    /// `session/request_permission` is surfaced over `permission_tx`.
    pub async fn connect(
        peer: JsonRpcPeer,
        incoming: Incoming,
        fs: Arc<dyn FsHost>,
        permission_tx: mpsc::Sender<PermissionPrompt>,
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

        let Incoming {
            requests,
            notifications,
        } = incoming;
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let router = executor.spawn(route_notifications(notifications, Arc::clone(&routes)));
        let request_router = executor.spawn(route_requests(
            requests,
            fs,
            permission_tx,
            executor.clone(),
        ));
        Ok(Self {
            peer,
            executor,
            cwd: cwd.into(),
            routes,
            _router: router,
            _request_router: request_router,
        })
    }
}

/// Drain the connection's inbound notifications, demux each
/// `session/update`, and forward the resulting [`AgentMessage`] to the
/// matching session's channel. Other notifications, and updates with no
/// host-facing event, are dropped.
async fn route_notifications(
    mut notifications: mpsc::UnboundedReceiver<IncomingNotification>,
    routes: Routes,
) {
    while let Some(notif) = notifications.recv().await {
        if notif.method != SESSION_UPDATE {
            continue;
        }
        let Some(params) = notif.params else {
            continue;
        };
        let Ok(update) = serde_json::from_value::<SessionUpdateNotification>(params) else {
            continue;
        };
        let Some(message) = demux_session_update(&update.update) else {
            continue;
        };
        if let Some(tx) = routes.lock().expect("routes mutex").get(&update.session_id) {
            let _ = tx.send(message);
        }
    }
}

/// Drain the connection's inbound requests: answer `fs/*` through `fs`,
/// surface each `session/request_permission` over `permission_tx`, and
/// reject every other method with a method-not-found error. The
/// permission handler awaits the user, so it is spawned to keep the loop
/// answering other requests meanwhile.
async fn route_requests(
    mut requests: mpsc::UnboundedReceiver<IncomingRequest>,
    fs: Arc<dyn FsHost>,
    permission_tx: mpsc::Sender<PermissionPrompt>,
    executor: Executor,
) {
    while let Some(req) = requests.recv().await {
        if let Some(response) = handle_fs_request(&req.method, req.params.as_ref(), &fs) {
            let _ = req.respond(response);
        } else if req.method == SESSION_REQUEST_PERMISSION {
            executor
                .spawn(handle_permission_request(req, permission_tx.clone()))
                .detach();
        } else {
            let rejection = Err(method_not_found(&req.method));
            let _ = req.respond(rejection);
        }
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
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes
            .lock()
            .expect("routes mutex")
            .insert(session.session_id.clone(), tx);
        Ok(Box::new(AcpSession::new(
            Arc::clone(&self.peer),
            self.executor.clone(),
            session.session_id,
            rx,
        )))
    }
}

/// One ACP conversation. Sends `session/prompt` and `session/cancel`
/// over the shared transport; receives the streamed agent output the
/// connection's router demuxes from `session/update`.
struct AcpSession {
    peer: Arc<JsonRpcPeer>,
    executor: Executor,
    session_id: String,
    alive: AtomicBool,
    /// The in-flight `session/prompt` request task. Held so the request
    /// frame is actually sent -- dropping a [`Task`] cancels it -- and
    /// replaced when the next prompt starts.
    turn: Mutex<Option<Task<()>>>,
    /// Demuxed `session/update` events for this session, fed by the
    /// connection's router. Async mutex so [`Self::recv`] can hold it
    /// across the await without blocking the scheduler.
    recv_rx: AsyncMutex<mpsc::UnboundedReceiver<AgentMessage>>,
}

impl AcpSession {
    fn new(
        peer: Arc<JsonRpcPeer>,
        executor: Executor,
        session_id: String,
        recv_rx: mpsc::UnboundedReceiver<AgentMessage>,
    ) -> Self {
        Self {
            peer,
            executor,
            session_id,
            alive: AtomicBool::new(true),
            turn: Mutex::new(None),
            recv_rx: AsyncMutex::new(recv_rx),
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
            // FIXME: the session/prompt response (turn-end stop reason) is
            // discarded; surface it as a turn-boundary AgentMessage if a
            // consumer needs an explicit end-of-turn signal.
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
        self.recv_rx.lock().await.recv().await
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
    use std::path::{Path, PathBuf};
    use stoat::host::{ApprovalDecision, FakeFs, ToolCallContent, ToolCallLocation, ToolKind};
    use stoat_agent_claude_code::jsonrpc::Incoming;
    use stoat_scheduler::TokioScheduler;
    use tokio::sync::mpsc;

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    fn fake_fs() -> Arc<dyn FsHost> {
        Arc::new(FakeFs::new())
    }

    /// A permission sender whose receiver is dropped, for tests that
    /// never trigger a permission request.
    fn fake_permission_tx() -> mpsc::Sender<PermissionPrompt> {
        mpsc::channel(1).0
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
        (Task<()>, JsonRpcPeer),
    ) {
        let ((client, client_in), (server, server_in)) = JsonRpcPeer::duplex(executor);
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
        let agent = fake_agent(executor, server_in, seen_tx);
        let conn = AcpConnection::connect(
            client,
            client_in,
            fake_fs(),
            fake_permission_tx(),
            executor.clone(),
            "/work",
        )
        .await
        .expect("connect");
        let init = seen_rx.recv().await.expect("initialize seen");
        assert_eq!(init.0, "initialize");
        (conn, seen_rx, (agent, server))
    }

    #[tokio::test]
    async fn connect_runs_initialize_with_client_capabilities() {
        let executor = executor();
        let ((client, client_in), (_server, server_in)) = JsonRpcPeer::duplex(&executor);
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
        let _agent = fake_agent(&executor, server_in, seen_tx);

        let _conn = AcpConnection::connect(
            client,
            client_in,
            fake_fs(),
            fake_permission_tx(),
            executor.clone(),
            "/work",
        )
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

    /// Push a `session/update` from the agent end and return the
    /// [`AgentMessage`] the session's `recv` yields.
    async fn recv_after_update(update: Value) -> AgentMessage {
        let executor = executor();
        let (conn, mut seen_rx, keep) = connected(&executor).await;
        let (_agent, server) = keep;
        let session = conn.new_session().await.expect("new_session");
        let _ = seen_rx.recv().await.expect("session/new seen");

        server
            .notify(
                "session/update",
                Some(json!({ "sessionId": "sess-1", "update": update })),
            )
            .expect("notify session/update");

        session.recv().await.expect("recv yields a message")
    }

    #[tokio::test]
    async fn agent_message_chunk_reaches_recv_as_text() {
        let message = recv_after_update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi there" },
        }))
        .await;
        match message {
            AgentMessage::Text { text } => assert_eq!(text, "hi there"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_thought_chunk_reaches_recv_as_thinking() {
        let message = recv_after_update(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "pondering" },
        }))
        .await;
        match message {
            AgentMessage::Thinking { text, signature } => {
                assert_eq!(text, "pondering");
                assert!(signature.is_empty());
            },
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_call_maps_kind_title_diff_and_location() {
        let message = recv_after_update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "kind": "execute",
            "title": "Run tests",
            "status": "pending",
            "content": [{ "type": "diff", "path": "/a.rs", "oldText": "x", "newText": "y" }],
            "locations": [{ "path": "/a.rs", "line": 3 }],
        }))
        .await;
        match message {
            AgentMessage::ToolUse {
                id,
                kind,
                title,
                content,
                locations,
                ..
            } => {
                assert_eq!(id, "tc-1");
                assert_eq!(kind, ToolKind::Execute);
                assert_eq!(title, "Run tests");
                assert_eq!(
                    content,
                    vec![ToolCallContent::Diff {
                        path: PathBuf::from("/a.rs"),
                        old_text: Some("x".to_string()),
                        new_text: "y".to_string(),
                    }]
                );
                assert_eq!(
                    locations,
                    vec![ToolCallLocation {
                        path: PathBuf::from("/a.rs"),
                        line: Some(3),
                    }]
                );
            },
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn current_mode_update_maps_to_mode_changed() {
        let message = recv_after_update(json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": "plan",
        }))
        .await;
        match message {
            AgentMessage::ModeChanged { mode } => assert_eq!(mode, "plan"),
            other => panic!("expected ModeChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_maps_entries() {
        let message = recv_after_update(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Write the parser", "priority": "high", "status": "in_progress" },
            ],
        }))
        .await;
        match message {
            AgentMessage::Plan { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].content, "Write the parser");
                assert_eq!(entries[0].priority, "high");
            },
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    /// Connect with `fs` and `permission_tx` injected and return the
    /// agent-end peer (so a test can send agent->client requests) plus the
    /// handles to keep alive.
    async fn connected_with_fs(
        executor: &Executor,
        fs: Arc<dyn FsHost>,
        permission_tx: mpsc::Sender<PermissionPrompt>,
    ) -> (JsonRpcPeer, (Task<()>, AcpConnection)) {
        let ((client, client_in), (server, server_in)) = JsonRpcPeer::duplex(executor);
        let (seen_tx, _seen) = mpsc::unbounded_channel();
        let agent = fake_agent(executor, server_in, seen_tx);
        let conn = AcpConnection::connect(
            client,
            client_in,
            fs,
            permission_tx,
            executor.clone(),
            "/work",
        )
        .await
        .expect("connect");
        (server, (agent, conn))
    }

    #[tokio::test]
    async fn agent_fs_read_request_is_answered_from_fs() {
        let executor = executor();
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        fs.write(Path::new("/greeting.txt"), b"hi there").unwrap();
        let (server, _keep) = connected_with_fs(&executor, fs, fake_permission_tx()).await;

        let response = server
            .request(
                "fs/read_text_file",
                Some(json!({ "path": "/greeting.txt" })),
            )
            .await
            .expect("fs/read response");
        assert_eq!(response, json!({ "content": "hi there" }));
    }

    #[tokio::test]
    async fn agent_fs_write_request_persists_to_fs() {
        let executor = executor();
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let (server, _keep) =
            connected_with_fs(&executor, Arc::clone(&fs), fake_permission_tx()).await;

        server
            .request(
                "fs/write_text_file",
                Some(json!({ "path": "/out.txt", "content": "saved" })),
            )
            .await
            .expect("fs/write response");

        let mut buf = Vec::new();
        fs.read(Path::new("/out.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"saved");
    }

    #[tokio::test]
    async fn agent_unknown_request_is_rejected() {
        let executor = executor();
        let (server, _keep) = connected_with_fs(&executor, fake_fs(), fake_permission_tx()).await;

        let result = server
            .request("fs/chmod", Some(json!({ "path": "/x" })))
            .await;
        assert!(result.is_err(), "unknown method should be rejected");
    }

    fn permission_request() -> Value {
        json!({
            "sessionId": "sess-1",
            "toolCall": { "toolCallId": "tc-1", "title": "Bash", "rawInput": "ls" },
            "options": [
                { "optionId": "ok", "kind": "allow_once" },
                { "optionId": "no", "kind": "reject_once" },
            ],
        })
    }

    #[tokio::test]
    async fn agent_permission_request_returns_user_choice() {
        let executor = executor();
        let (permission_tx, mut permission_rx) = mpsc::channel(4);
        let (server, _keep) = connected_with_fs(&executor, fake_fs(), permission_tx).await;

        let user = executor.spawn(async move {
            let prompt = permission_rx.recv().await.expect("prompt surfaced");
            assert_eq!(prompt.tool, "Bash");
            assert_eq!(prompt.input, "ls");
            let _ = prompt.response_tx.send(ApprovalDecision::AllowOnce);
        });

        let response = server
            .request(SESSION_REQUEST_PERMISSION, Some(permission_request()))
            .await
            .expect("permission response");
        assert_eq!(
            response,
            json!({ "outcome": { "outcome": "selected", "optionId": "ok" } })
        );
        user.await;
    }

    #[tokio::test]
    async fn agent_permission_request_cancelled_when_dismissed() {
        let executor = executor();
        let (permission_tx, mut permission_rx) = mpsc::channel(4);
        let (server, _keep) = connected_with_fs(&executor, fake_fs(), permission_tx).await;

        let user = executor.spawn(async move {
            let prompt = permission_rx.recv().await.expect("prompt surfaced");
            drop(prompt.response_tx);
        });

        let response = server
            .request(SESSION_REQUEST_PERMISSION, Some(permission_request()))
            .await
            .expect("permission response");
        assert_eq!(response, json!({ "outcome": { "outcome": "cancelled" } }));
        user.await;
    }
}
