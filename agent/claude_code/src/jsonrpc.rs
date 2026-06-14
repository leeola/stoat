//! Reusable, fakeable piped-stdio JSON-RPC 2.0 transport.
//!
//! [`JsonRpcPeer`] is a bidirectional, message-agnostic JSON-RPC 2.0
//! endpoint over newline-delimited frames. It correlates outbound
//! requests with their responses, surfaces inbound requests (each with a
//! [`Responder`]) and notifications on channels, and routes every task
//! through the injected [`Executor`] -- never `tokio::spawn`.
//!
//! Two constructors share one core ([`JsonRpcPeer::over_lines`]):
//! [`JsonRpcPeer::duplex`] wires two peers together in memory with no
//! subprocess (the path tests drive), and the subprocess constructor
//! frames a child's stdio. Payloads are opaque [`serde_json::Value`]s, so
//! a typed message schema layers on top without the transport knowing it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::{Location, OptionExt, ResultExt, Snafu};
use std::{
    collections::HashMap,
    io,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use stoat_log::TextProtoLog;
use stoat_scheduler::{Executor, Task};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStderr, Command},
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};

/// JSON-RPC 2.0 error object returned by a peer in place of a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Failure modes of [`JsonRpcPeer`] request/notify/respond calls.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum TransportError {
    #[snafu(display("failed to serialize JSON-RPC message"))]
    Encode {
        source: serde_json::Error,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("JSON-RPC transport is closed"))]
    Closed {
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("JSON-RPC transport closed before a response arrived"))]
    Canceled {
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("JSON-RPC peer returned error {}: {}", error.code, error.message))]
    Rpc {
        error: RpcError,
        #[snafu(implicit)]
        location: Location,
    },
}

/// Failure modes of [`JsonRpcPeer::spawn`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SpawnError {
    #[snafu(display("failed to spawn JSON-RPC child process"))]
    Spawn {
        source: io::Error,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("spawned child is missing a stdio handle"))]
    Stdio {
        #[snafu(implicit)]
        location: Location,
    },
}

/// Outbound requests awaiting their correlated response, keyed by the
/// `u64` id assigned when the request was sent.
type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A bidirectional JSON-RPC 2.0 endpoint. Cheap to share behind an
/// [`Arc`]: every method takes `&self`. The transport stays live while
/// this value is held; dropping it cancels the dispatch loop and (for a
/// spawned child) the stdio tasks.
pub struct JsonRpcPeer {
    outgoing: UnboundedSender<String>,
    next_id: AtomicU64,
    pending: PendingResponses,
    /// Dispatch loop plus, for a spawned child, the stdio pump tasks.
    /// Held only to keep them scheduled; never awaited.
    _tasks: Vec<Task<()>>,
    /// The spawned child, held so its `kill_on_drop` fires when this peer
    /// drops. `None` for the in-memory duplex.
    _child: Option<Child>,
}

/// The inbound half of a [`JsonRpcPeer`]: requests the peer must answer
/// (each carries a [`Responder`]) and fire-and-forget notifications.
/// Drain both with `recv().await`.
pub struct Incoming {
    pub requests: UnboundedReceiver<IncomingRequest>,
    pub notifications: UnboundedReceiver<IncomingNotification>,
}

/// An inbound request awaiting a response. The caller reads [`method`]
/// and [`params`], then calls [`respond`] exactly once; dropping it
/// without responding leaves the remote request unanswered.
///
/// [`method`]: IncomingRequest::method
/// [`params`]: IncomingRequest::params
/// [`respond`]: IncomingRequest::respond
pub struct IncomingRequest {
    pub method: String,
    pub params: Option<Value>,
    responder: Responder,
}

/// An inbound notification: a method + params with no reply expected.
pub struct IncomingNotification {
    pub method: String,
    pub params: Option<Value>,
}

/// The reply channel for one [`IncomingRequest`], carrying the request id
/// to echo. Reached only through [`IncomingRequest::respond`].
struct Responder {
    id: Value,
    outgoing: UnboundedSender<String>,
}

impl JsonRpcPeer {
    /// Send a request and await the correlated response. Resolves to the
    /// peer's result value, or [`TransportError::Rpc`] when the peer
    /// answers with an error, [`TransportError::Closed`] when the request
    /// cannot be enqueued, or [`TransportError::Canceled`] when the
    /// transport closes before a reply arrives.
    pub async fn request(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = serde_json::to_string(&OutRequest {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.into(),
            params,
        })
        .context(EncodeSnafu)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending mutex").insert(id, tx);
        if self.outgoing.send(line).is_err() {
            self.pending.lock().expect("pending mutex").remove(&id);
            return ClosedSnafu.fail();
        }

        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => RpcSnafu { error }.fail(),
            Err(_) => CanceledSnafu.fail(),
        }
    }

    /// Send a notification (no id, no reply). Returns once the frame is
    /// enqueued; errors only when serialization fails or the transport is
    /// closed.
    pub fn notify(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<(), TransportError> {
        let line = serde_json::to_string(&OutNotification {
            jsonrpc: JSONRPC_VERSION,
            method: method.into(),
            params,
        })
        .context(EncodeSnafu)?;
        self.outgoing.send(line).map_err(|_| ClosedSnafu.build())
    }

    /// Two peers wired back-to-back in memory: each peer's outbound
    /// frames are the other's inbound frames. No subprocess, no pipes --
    /// the path tests use to exercise the protocol deterministically.
    pub fn duplex(executor: &Executor) -> ((JsonRpcPeer, Incoming), (JsonRpcPeer, Incoming)) {
        let (a_to_b_tx, a_to_b_rx) = mpsc::unbounded_channel();
        let (b_to_a_tx, b_to_a_rx) = mpsc::unbounded_channel();
        let a = Self::over_lines(b_to_a_rx, a_to_b_tx, executor);
        let b = Self::over_lines(a_to_b_rx, b_to_a_tx, executor);
        (a, b)
    }

    /// Spawn `command` as a child with piped stdin/stdout/stderr and run
    /// the JSON-RPC protocol over its newline-framed stdio. Inbound stdout
    /// lines feed the dispatch loop; outbound frames are written to stdin
    /// with a trailing newline. When present, `tx_log`/`rx_log` capture a
    /// byte-faithful transcript of each direction; stderr is drained to the
    /// tracing log. The child is killed when the returned peer drops. All
    /// three stdio pumps and the dispatch loop run on `executor`.
    pub fn spawn(
        mut command: Command,
        executor: &Executor,
        tx_log: Option<Arc<TextProtoLog>>,
        rx_log: Option<Arc<TextProtoLog>>,
    ) -> Result<(JsonRpcPeer, Incoming), SpawnError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().context(SpawnSnafu)?;
        let stdin = child.stdin.take().context(StdioSnafu)?;
        let stdout = child.stdout.take().context(StdioSnafu)?;
        let stderr = child.stderr.take().context(StdioSnafu)?;

        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let read_task = executor.spawn(read_frames(stdout, incoming_tx, rx_log));
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let write_task = executor.spawn(write_frames(stdin, outgoing_rx, tx_log));
        let stderr_task = executor.spawn(drain_stderr(stderr));

        let (mut peer, incoming) = Self::over_lines(incoming_rx, outgoing_tx, executor);
        peer._tasks.extend([read_task, write_task, stderr_task]);
        peer._child = Some(child);
        Ok((peer, incoming))
    }

    /// Run the JSON-RPC protocol over a connected Unix-domain stream, framing
    /// each direction as newline-delimited JSON. Inbound lines feed the
    /// dispatch loop; outbound frames are written with a trailing newline.
    /// Both stream pumps and the dispatch loop run on `executor`; the stream
    /// closes when the returned peer drops.
    ///
    /// A `UnixStream` is symmetric, so this serves both the connecting client
    /// and each connection a [`serve_unix`] listener accepts.
    pub fn connect_unix(stream: UnixStream, executor: &Executor) -> (JsonRpcPeer, Incoming) {
        let (read_half, write_half) = stream.into_split();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let read_task = executor.spawn(read_frames(read_half, incoming_tx, None));
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let write_task = executor.spawn(write_frames(write_half, outgoing_rx, None));

        let (mut peer, incoming) = Self::over_lines(incoming_rx, outgoing_tx, executor);
        peer._tasks.extend([read_task, write_task]);
        (peer, incoming)
    }

    /// Build a peer over a line-framed transport: `incoming` yields one
    /// decoded JSON text per inbound frame, `outgoing` accepts one per
    /// outbound frame (the caller appends framing if a wire needs it; the
    /// duplex path does not). Spawns the dispatch loop on `executor`.
    pub(crate) fn over_lines(
        mut incoming: UnboundedReceiver<String>,
        outgoing: UnboundedSender<String>,
        executor: &Executor,
    ) -> (JsonRpcPeer, Incoming) {
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();

        let dispatch = executor.spawn({
            let pending = Arc::clone(&pending);
            let outgoing = outgoing.clone();
            async move {
                while let Some(line) = incoming.recv().await {
                    match parse_frame(&line) {
                        Parsed::Response { id, result } => {
                            match pending.lock().expect("pending mutex").remove(&id) {
                                Some(tx) => {
                                    let _ = tx.send(result);
                                },
                                None => tracing::debug!(id, "response with no pending request"),
                            }
                        },
                        Parsed::Request { id, method, params } => {
                            let responder = Responder {
                                id,
                                outgoing: outgoing.clone(),
                            };
                            let _ = req_tx.send(IncomingRequest {
                                method,
                                params,
                                responder,
                            });
                        },
                        Parsed::Notification { method, params } => {
                            let _ = notif_tx.send(IncomingNotification { method, params });
                        },
                        Parsed::Malformed => {
                            tracing::warn!(frame = %line, "discarding malformed JSON-RPC frame")
                        },
                    }
                }
                // Inbound closed: fail every in-flight request rather than
                // leave its caller awaiting a reply that can never arrive.
                pending.lock().expect("pending mutex").clear();
            }
        });

        let peer = JsonRpcPeer {
            outgoing,
            next_id: AtomicU64::new(1),
            pending,
            _tasks: vec![dispatch],
            _child: None,
        };
        (
            peer,
            Incoming {
                requests: req_rx,
                notifications: notif_rx,
            },
        )
    }
}

impl IncomingRequest {
    /// Answer this request, echoing its id. Call once; errors only when
    /// serialization fails or the transport is closed.
    pub fn respond(self, result: Result<Value, RpcError>) -> Result<(), TransportError> {
        self.responder.respond(result)
    }
}

impl Responder {
    fn respond(self, result: Result<Value, RpcError>) -> Result<(), TransportError> {
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        let line = serde_json::to_string(&OutResponse {
            jsonrpc: JSONRPC_VERSION,
            id: self.id,
            result,
            error,
        })
        .context(EncodeSnafu)?;
        self.outgoing.send(line).map_err(|_| ClosedSnafu.build())
    }
}

/// JSON-RPC 2.0 reserved code for an unrecognized method.
const METHOD_NOT_FOUND: i64 = -32601;

/// Bind a listener at `path`, clearing a stale socket left by a crashed
/// process so a fresh process can take over the well-known path.
///
/// Creates the parent directory if missing. When the path is already bound,
/// probes it with a blocking connect: a successful connect means a live server
/// still holds it (returns [`io::ErrorKind::AddrInUse`]), while a refused
/// connect means the socket is stale and is removed before rebinding. Returns
/// a std listener so binding needs no async runtime; [`serve_unix`] registers
/// it with the reactor inside its accept task.
pub fn bind_unix(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match StdUnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if StdUnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "stoat app socket is already bound by a live process",
                ));
            }
            std::fs::remove_file(path)?;
            StdUnixListener::bind(path)
        },
        Err(err) => Err(err),
    }
}

/// Bind the singleton app IPC socket at `path` and accept client connections
/// on `executor`, returning the task that owns the accept loop.
///
/// Every inbound request is answered with a method-not-found error and
/// notifications are dropped until session verbs are registered -- the
/// correct reply for a server exposing no methods yet. Binding (and its
/// stale-socket replacement, see [`bind_unix`]) happens synchronously so a
/// live-socket conflict surfaces to the caller; the returned task must be held
/// for the process lifetime, as dropping it stops accepting and releases the
/// socket.
pub fn serve_unix(path: &Path, executor: &Executor) -> io::Result<Task<()>> {
    let listener = bind_unix(path)?;
    listener.set_nonblocking(true)?;

    let executor = executor.clone();
    let task = executor.spawn({
        let executor = executor.clone();
        async move {
            let listener = match UnixListener::from_std(listener) {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::error!(%err, "registering app socket with the reactor failed");
                    return;
                },
            };
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let (peer, incoming) = JsonRpcPeer::connect_unix(stream, &executor);
                        executor.spawn(serve_connection(peer, incoming)).detach();
                    },
                    Err(err) => {
                        tracing::warn!(%err, "stoat app socket accept loop stopped");
                        break;
                    },
                }
            }
        }
    });
    Ok(task)
}

/// Hold one accepted connection's peer alive and answer every request with a
/// method-not-found error, draining notifications, until the client
/// disconnects. Session verbs replace this default handler in a later change.
async fn serve_connection(_peer: JsonRpcPeer, incoming: Incoming) {
    let Incoming {
        mut requests,
        mut notifications,
    } = incoming;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => {
                let message = format!("method not found: {}", request.method);
                let _ = request.respond(Err(RpcError {
                    code: METHOD_NOT_FOUND,
                    message,
                    data: None,
                }));
            },
            Some(notification) = notifications.recv() => {
                tracing::debug!(
                    method = %notification.method,
                    "app socket notification ignored (no verbs registered)",
                );
            },
            else => break,
        }
    }
}

const JSONRPC_VERSION: &str = "2.0";

#[derive(Serialize)]
struct OutRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct OutNotification {
    jsonrpc: &'static str,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct OutResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct WireFrame {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Classification of one inbound frame. Outbound request ids are a `u64`
/// counter, so a response id that is not a `u64` cannot match anything
/// we sent and is treated as malformed.
enum Parsed {
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
    Malformed,
}

fn parse_frame(line: &str) -> Parsed {
    let Ok(frame) = serde_json::from_str::<WireFrame>(line) else {
        return Parsed::Malformed;
    };
    let WireFrame {
        id,
        method,
        params,
        result,
        error,
    } = frame;
    match (method, id) {
        // method + id with no result/error is an inbound request; a
        // response carries result/error and no method.
        (Some(method), Some(id)) if result.is_none() && error.is_none() => {
            Parsed::Request { id, method, params }
        },
        (Some(method), None) => Parsed::Notification { method, params },
        (None, Some(id)) => match id.as_u64() {
            Some(id) => Parsed::Response {
                id,
                result: match error {
                    Some(error) => Err(error),
                    None => Ok(result.unwrap_or(Value::Null)),
                },
            },
            None => Parsed::Malformed,
        },
        _ => Parsed::Malformed,
    }
}

async fn read_frames<R: AsyncRead + Unpin>(
    reader: R,
    incoming: UnboundedSender<String>,
    rx_log: Option<Arc<TextProtoLog>>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(log) = &rx_log {
                    log.record(trimmed);
                }
                if incoming.send(trimmed.to_string()).is_err() {
                    break;
                }
            },
            Err(err) => {
                tracing::error!(%err, "JSON-RPC stdout read failed");
                break;
            },
        }
    }
}

async fn write_frames<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut outgoing: UnboundedReceiver<String>,
    tx_log: Option<Arc<TextProtoLog>>,
) {
    while let Some(line) = outgoing.recv().await {
        if let Some(log) = &tx_log {
            log.record(&line);
        }
        if writer.write_all(line.as_bytes()).await.is_err()
            || writer.write_all(b"\n").await.is_err()
            || writer.flush().await.is_err()
        {
            break;
        }
    }
}

async fn drain_stderr(stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if !trimmed.is_empty() {
                    tracing::warn!(stderr = %trimmed, "JSON-RPC child stderr");
                }
            },
            Err(err) => {
                tracing::error!(%err, "JSON-RPC stderr read failed");
                break;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_scheduler::TokioScheduler;

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    /// Drive `incoming.requests`, answering each by calling `handler` on
    /// (method, params). Spawns onto `executor` so the test's request
    /// future and the responder run concurrently.
    fn answer_requests(
        executor: &Executor,
        mut requests: UnboundedReceiver<IncomingRequest>,
        handler: impl Fn(&str, Option<&Value>) -> Result<Value, RpcError> + Send + 'static,
    ) -> Task<()> {
        executor.spawn(async move {
            while let Some(req) = requests.recv().await {
                let result = handler(&req.method, req.params.as_ref());
                let _ = req.respond(result);
            }
        })
    }

    #[tokio::test]
    async fn request_resolves_to_peer_result() {
        let executor = executor();
        let ((client, _client_in), (_server, server_in)) = JsonRpcPeer::duplex(&executor);
        let _answer = answer_requests(&executor, server_in.requests, |method, params| {
            assert_eq!(method, "echo");
            Ok(params.cloned().unwrap_or(Value::Null))
        });

        let result = client
            .request("echo", Some(serde_json::json!({ "v": 7 })))
            .await
            .expect("request succeeds");
        assert_eq!(result, serde_json::json!({ "v": 7 }));
    }

    #[tokio::test]
    async fn request_surfaces_rpc_error() {
        let executor = executor();
        let ((client, _client_in), (_server, server_in)) = JsonRpcPeer::duplex(&executor);
        let _answer = answer_requests(&executor, server_in.requests, |_, _| {
            Err(RpcError {
                code: -32601,
                message: "method not found".to_string(),
                data: None,
            })
        });

        let err = client
            .request("nope", None)
            .await
            .expect_err("request fails");
        match err {
            TransportError::Rpc { error, .. } => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "method not found");
            },
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notification_is_delivered() {
        let executor = executor();
        let ((client, _client_in), (_server, mut server_in)) = JsonRpcPeer::duplex(&executor);

        client
            .notify("tick", Some(serde_json::json!({ "n": 1 })))
            .expect("notify enqueues");

        let notif = server_in
            .notifications
            .recv()
            .await
            .expect("notification received");
        assert_eq!(notif.method, "tick");
        assert_eq!(notif.params, Some(serde_json::json!({ "n": 1 })));
    }

    #[tokio::test]
    async fn concurrent_requests_correlate_by_id() {
        let executor = executor();
        let ((client, _client_in), (_server, server_in)) = JsonRpcPeer::duplex(&executor);
        // Echo the request's `id` param so each caller can verify it got
        // its own reply rather than a transposed one.
        let _answer = answer_requests(&executor, server_in.requests, |_, params| {
            Ok(params.cloned().unwrap_or(Value::Null))
        });

        let a = client.request("echo", Some(serde_json::json!({ "id": "a" })));
        let b = client.request("echo", Some(serde_json::json!({ "id": "b" })));
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.expect("a"), serde_json::json!({ "id": "a" }));
        assert_eq!(b.expect("b"), serde_json::json!({ "id": "b" }));
    }

    #[tokio::test]
    async fn both_ends_can_originate_requests() {
        let executor = executor();
        let ((client, client_in), (server, server_in)) = JsonRpcPeer::duplex(&executor);
        let _server_answers = answer_requests(&executor, server_in.requests, |_, _| {
            Ok(Value::String("from-server".to_string()))
        });
        let _client_answers = answer_requests(&executor, client_in.requests, |_, _| {
            Ok(Value::String("from-client".to_string()))
        });

        let to_server = client.request("ask", None).await.expect("client->server");
        let to_client = server.request("ask", None).await.expect("server->client");
        assert_eq!(to_server, Value::String("from-server".to_string()));
        assert_eq!(to_client, Value::String("from-client".to_string()));
    }

    #[tokio::test]
    async fn request_is_canceled_when_peer_drops() {
        let executor = executor();
        let ((client, _client_in), (server, server_in)) = JsonRpcPeer::duplex(&executor);
        // Drop the server end without answering; its dispatch loop ends,
        // closing the line the client's response would travel.
        drop(server);
        drop(server_in);

        let err = client
            .request("orphan", None)
            .await
            .expect_err("request cannot complete");
        assert!(
            matches!(
                err,
                TransportError::Canceled { .. } | TransportError::Closed { .. }
            ),
            "expected Canceled/Closed, got {err:?}",
        );
    }

    #[tokio::test]
    async fn spawn_round_trips_a_frame_through_child_stdio() {
        if !Path::new("/bin/sh").exists() {
            eprintln!("skipping: no /bin/sh");
            return;
        }
        let executor = executor();
        // `head -n 1` reads one stdin line, echoes it, and exits (flushing
        // on exit), so a frame the peer sends returns as an inbound frame --
        // exercising the spawn + stdin-write + stdout-read pumps end to end.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("head -n 1");
        let (peer, mut incoming) =
            JsonRpcPeer::spawn(command, &executor, None, None).expect("spawn child");

        peer.notify("ping", Some(serde_json::json!({ "n": 1 })))
            .expect("notify enqueues");

        let notif = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            incoming.notifications.recv(),
        )
        .await
        .expect("frame echoed within timeout")
        .expect("frame echoed back through child stdio");
        assert_eq!(notif.method, "ping");
        assert_eq!(notif.params, Some(serde_json::json!({ "n": 1 })));
    }

    #[tokio::test]
    async fn serve_unix_round_trips_and_answers_method_not_found() {
        let executor = executor();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.sock");

        let _server = serve_unix(&path, &executor).expect("bind app socket");
        let stream = UnixStream::connect(&path).await.expect("client connects");
        let (client, _client_in) = JsonRpcPeer::connect_unix(stream, &executor);

        let err = client
            .request("open_file", None)
            .await
            .expect_err("server exposes no verbs yet");
        match err {
            TransportError::Rpc { error, .. } => assert_eq!(error.code, METHOD_NOT_FOUND),
            other => panic!("expected Rpc method-not-found, got {other:?}"),
        }
    }

    #[test]
    fn bind_unix_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.sock");

        let listener = bind_unix(&path).expect("first bind");
        drop(listener);
        assert!(
            path.exists(),
            "socket file lingers after the listener drops"
        );

        bind_unix(&path).expect("stale socket is replaced");
    }

    #[test]
    fn bind_unix_rejects_a_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.sock");

        let _live = bind_unix(&path).expect("first bind");
        let err = bind_unix(&path).expect_err("a live socket is not replaced");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }
}
