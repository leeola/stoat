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
use snafu::{Location, ResultExt, Snafu};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use stoat_scheduler::{Executor, Task};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
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
}
