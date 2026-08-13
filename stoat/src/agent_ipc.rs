//! Per-session IPC server for Claude hook events.
//!
//! Each owned Claude subshell is spawned with `STOAT_AGENT_SOCK` pointing at a
//! per-session Unix socket (see [`crate::run::agent_socket_path`]). This module
//! binds that socket, reads newline-framed JSON hook events from connecting
//! clients, and forwards them to the render process's event loop as
//! [`AgentEvent`]s, which it applies to the owning workspace's
//! [`AgentStatus`](crate::agent_status::AgentStatus).

use crate::{
    agent_status::AgentHookEvent, app::Stoat, host::LanguageServerFeature, workspace::WorkspaceUid,
};
use lsp_types::{HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{mpsc::Sender, oneshot},
};

/// A hook event tagged with the session it belongs to.
///
/// The socket is per-session, so [`serve_agent_hooks`] stamps each decoded
/// [`AgentHookEvent`] with its `uid` before forwarding. The event loop routes
/// by `uid` to the matching workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub uid: WorkspaceUid,
    pub event: AgentHookEvent,
}

/// A control request from an owned agent that expects a reply.
///
/// Unlike [`AgentEvent`], a control request carries a [`oneshot::Sender`] the
/// event loop fires when the requested interaction finishes, so it cannot ride
/// the serde-and-`Clone` [`AgentHookEvent`] path. The event loop routes it by
/// `uid` to the owning workspace.
pub enum AgentControl {
    /// Open `path` as a buffer in the session's workspace and keep the agent
    /// blocked until that buffer (or its hosting pane) closes. The close path
    /// fires `done`, which unblocks the parked socket connection so the agent's
    /// `$EDITOR` invocation returns.
    OpenEditor {
        uid: WorkspaceUid,
        path: PathBuf,
        done: oneshot::Sender<()>,
    },
    /// Answer a live-session [`AgentQuery`] and fire `reply` with the JSON
    /// result. The connection stays open afterward, so several queries ride one
    /// connection, unlike the park-and-return [`Self::OpenEditor`].
    Query {
        uid: WorkspaceUid,
        request: AgentQuery,
        reply: oneshot::Sender<Value>,
    },
}

/// A read-only interrogation of live session state, answered by the event loop.
///
/// Separate from the [`AgentRequest`] wire form so the control channel carries
/// only genuine queries. The `open-editor` request is a blocking interaction
/// routed through [`AgentControl::OpenEditor`] and never appears here.
#[derive(Debug, PartialEq)]
pub enum AgentQuery {
    /// LSP host liveness plus the server's serialized capabilities.
    LspStatus,
    /// Diagnostics for `path`, or for every tracked path when `None`.
    Diagnostics { path: Option<PathBuf> },
    /// Hover at an LSP UTF-16 `line`/`col` within `path`.
    Hover { path: PathBuf, line: u32, col: u32 },
}

/// A request decoded from one socket line.
///
/// Tagged on `req` so it stays disjoint from the `hook`-tagged
/// [`AgentHookEvent`] wire form: a hook line has no `req` field and fails to
/// decode here, so [`serve_connection`] can try a request decode first and fall
/// through to the hook path.
#[derive(Debug, Deserialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
enum AgentRequest {
    /// `{"req":"open-editor","path":"..."}`.
    OpenEditor { path: PathBuf },
    /// `{"req":"lsp-status"}`.
    LspStatus,
    /// `{"req":"diagnostics"}` or `{"req":"diagnostics","path":"..."}`.
    Diagnostics { path: Option<PathBuf> },
    /// `{"req":"hover","path":"...","line":N,"col":N}`. `line`/`col` are LSP
    /// UTF-16 positions, forwarded to the server unconverted.
    Hover { path: PathBuf, line: u32, col: u32 },
}

/// Bind the per-session socket at `socket_path` and forward decoded hook events
/// to `tx` and decoded control requests to `control_tx`, until the listener
/// fails or the receiver is dropped.
///
/// Spawned on the render process's executor. A stale socket file at the path is
/// removed before binding. Bind and accept failures are logged and stop the
/// server, leaving the app running without hook status for that session.
pub async fn serve_agent_hooks(
    socket_path: PathBuf,
    uid: WorkspaceUid,
    tx: Sender<AgentEvent>,
    control_tx: Sender<AgentControl>,
) {
    if let Some(parent) = socket_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::remove_file(&socket_path).await;

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(%err, ?socket_path, "agent hook server failed to bind");
            return;
        },
    };

    loop {
        match listener.accept().await {
            Ok((stream, _)) => serve_connection(stream, uid, &tx, &control_tx).await,
            Err(err) => {
                tracing::warn!(%err, "agent hook server stopped accepting");
                break;
            },
        }
        if tx.is_closed() {
            break;
        }
    }
}

/// Forward one client connection's hook events to `tx` and its open-editor
/// requests to `control_tx`, replying to the latter once the editor closes.
///
/// Each line is tried as an [`AgentRequest`] first, then as an
/// [`AgentHookEvent`]. An open-editor request parks the connection until the
/// event loop fires its waiter, then writes an `editor-closed` reply and
/// returns. Otherwise returns when the client disconnects, a read fails, or a
/// receiver is dropped. Blank lines are ignored and malformed lines are logged
/// and skipped, so one bad line never tears down the connection.
async fn serve_connection<R>(
    stream: R,
    uid: WorkspaceUid,
    tx: &Sender<AgentEvent>,
    control_tx: &Sender<AgentControl>,
) where
    R: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, "agent hook read failed");
                return;
            },
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(request) = serde_json::from_str::<AgentRequest>(trimmed) {
            let query = match request {
                AgentRequest::OpenEditor { path } => {
                    let (done_tx, done_rx) = oneshot::channel();
                    if control_tx
                        .send(AgentControl::OpenEditor {
                            uid,
                            path,
                            done: done_tx,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = done_rx.await;
                    let _ = write_half
                        .write_all(b"{\"reply\":\"editor-closed\"}\n")
                        .await;
                    return;
                },
                AgentRequest::LspStatus => AgentQuery::LspStatus,
                AgentRequest::Diagnostics { path } => AgentQuery::Diagnostics { path },
                AgentRequest::Hover { path, line, col } => AgentQuery::Hover { path, line, col },
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            if control_tx
                .send(AgentControl::Query {
                    uid,
                    request: query,
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return;
            }
            let value = match reply_rx.await {
                Ok(value) => value,
                Err(_) => return,
            };
            let mut encoded = match serde_json::to_vec(&value) {
                Ok(encoded) => encoded,
                Err(err) => {
                    tracing::warn!(%err, "failed to encode query reply");
                    continue;
                },
            };
            encoded.push(b'\n');
            if write_half.write_all(&encoded).await.is_err() {
                return;
            }
            continue;
        }

        match parse_hook_line(trimmed) {
            Ok(event) => {
                if tx.send(AgentEvent { uid, event }).await.is_err() {
                    return;
                }
            },
            Err(err) => tracing::warn!(%err, line = %trimmed, "ignored malformed hook line"),
        }
    }
}

/// Decode one newline-stripped JSON hook line into an [`AgentHookEvent`].
fn parse_hook_line(line: &str) -> Result<AgentHookEvent, serde_json::Error> {
    serde_json::from_str(line)
}

/// Answer a runtime [`AgentQuery`] from live session state, firing `reply` with
/// the JSON result.
///
/// `lsp-status` and `diagnostics` reply synchronously. `hover` requires the path
/// to be open in the `uid` session (otherwise `{"error":"not open"}`) and runs
/// the request on a detached task so the event loop never blocks on the server.
pub(crate) fn answer_agent_query(
    stoat: &mut Stoat,
    uid: WorkspaceUid,
    request: AgentQuery,
    reply: oneshot::Sender<Value>,
) {
    match request {
        AgentQuery::LspStatus => {
            let servers: Vec<Value> = stoat
                .lsp_registry
                .named_hosts()
                .into_iter()
                .filter(|(_, host)| !host.is_noop())
                .map(|(name, host)| {
                    let capabilities =
                        serde_json::to_value(&*host.capabilities()).unwrap_or(Value::Null);
                    json!({ "name": name, "capabilities": capabilities })
                })
                .collect();
            let _ = reply.send(json!({
                "active": !servers.is_empty(),
                "spawn_attempted": stoat.lsp_registry.spawn_attempted_any(),
                "servers": servers,
            }));
        },
        AgentQuery::Diagnostics { path } => {
            let value = match path {
                Some(path) => {
                    serde_json::to_value(stoat.diagnostics.get(&path)).unwrap_or(Value::Null)
                },
                None => Value::Array(
                    stoat
                        .diagnostics
                        .iter()
                        .map(|(path, diagnostics)| json!({ "path": path, "diagnostics": diagnostics }))
                        .collect(),
                ),
            };
            let _ = reply.send(value);
        },
        AgentQuery::Hover { path, line, col } => {
            let buffer_id = stoat
                .workspaces
                .iter()
                .find(|(_, ws)| ws.uid == uid)
                .and_then(|(_, ws)| ws.buffers.id_for_path(&path));
            let Some(buffer_id) = buffer_id.filter(|id| stoat.lsp_opened.contains(id)) else {
                let _ = reply.send(json!({ "error": "not open" }));
                return;
            };
            let Some(uri) = crate::action_handlers::lsp::path_to_uri(&path) else {
                let _ = reply.send(json!({ "error": "invalid path" }));
                return;
            };

            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line,
                        character: col,
                    },
                },
                work_done_progress_params: Default::default(),
            };
            let lsp =
                crate::lsp::hosts::lsp_for_feature(stoat, buffer_id, LanguageServerFeature::Hover);
            stoat
                .executor
                .spawn(async move {
                    let value = match lsp.hover(params).await {
                        Ok(Some(hover)) => serde_json::to_value(&hover).unwrap_or(Value::Null),
                        Ok(None) => Value::Null,
                        Err(err) => json!({ "error": err.to_string() }),
                    };
                    let _ = reply.send(value);
                })
                .detach();
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{install_two_servers, open_buffer, seed},
        test_harness::TestHarness,
    };

    #[test]
    fn lsp_status_lists_each_running_server() {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        let _ = install_two_servers(
            &mut h,
            ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
        );

        let uid = h.stoat.active_workspace().uid();
        let (tx, mut rx) = oneshot::channel();
        answer_agent_query(&mut h.stoat, uid, AgentQuery::LspStatus, tx);
        let value = rx.try_recv().expect("lsp-status reply");

        assert_eq!(value["active"], serde_json::json!(true));
        let names: Vec<&str> = value["servers"]
            .as_array()
            .expect("servers array")
            .iter()
            .map(|s| s["name"].as_str().expect("server name"))
            .collect();
        assert!(
            names.contains(&"primary") && names.contains(&"secondary"),
            "servers listed: {names:?}",
        );
    }

    #[test]
    fn query_diagnostics_returns_seeded_set() {
        use lsp_types::Diagnostic;

        let mut h = TestHarness::with_size(40, 10);
        let path = PathBuf::from("/proj/a.rs");
        let diagnostic = Diagnostic {
            message: "boom".into(),
            ..Default::default()
        };
        h.seed_diagnostics(path.clone(), vec![diagnostic.clone()]);

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Diagnostics { path: Some(path) },
            reply: reply_tx,
        });

        let value = reply_rx.try_recv().expect("synchronous diagnostics reply");
        let got: Vec<Diagnostic> = serde_json::from_value(value).unwrap();
        assert_eq!(got, vec![diagnostic]);
    }

    #[test]
    fn query_hover_returns_fake_hover() {
        use lsp_types::{Hover, HoverContents};

        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 1, "hover text");

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path: path.clone(),
                line: 0,
                col: 1,
            },
            reply: reply_tx,
        });
        h.settle();

        let value = reply_rx.try_recv().expect("hover reply");
        let hover: Hover = serde_json::from_value(value).unwrap();
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover contents");
        };
        assert_eq!(markup.value, "hover text");
    }

    #[test]
    fn query_hover_on_unopened_path_replies_error() {
        let mut h = TestHarness::with_size(40, 10);
        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path: PathBuf::from("/nope.rs"),
                line: 0,
                col: 0,
            },
            reply: reply_tx,
        });

        let value = reply_rx.try_recv().expect("synchronous error reply");
        assert_eq!(value, serde_json::json!({ "error": "not open" }));
    }

    #[test]
    fn wire_form_round_trips() {
        let event = AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"hook":"pre-tool-use","tool":"Bash"}"#);
        assert_eq!(parse_hook_line(&json).unwrap(), event);
    }

    #[test]
    fn unit_variant_decodes_from_tag_only() {
        assert_eq!(
            parse_hook_line(r#"{"hook":"session-end"}"#).unwrap(),
            AgentHookEvent::SessionEnd
        );
    }

    /// Wrap a byte slice as a read-write stream for [`serve_connection`], which
    /// needs `AsyncWrite` to reply to requests. Hook-only inputs never write, so
    /// the write half discards into a sink.
    fn read_only(input: &'static [u8]) -> impl AsyncRead + AsyncWrite + Unpin {
        tokio::io::join(input, tokio::io::sink())
    }

    #[tokio::test]
    async fn connection_forwards_each_hook_line() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let uid = WorkspaceUid(7);
        let input: &[u8] =
            b"{\"hook\":\"pre-tool-use\",\"tool\":\"Bash\"}\n\n{\"hook\":\"stop\"}\n";

        serve_connection(read_only(input), uid, &tx, &control_tx).await;
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            assert_eq!(ev.uid, uid);
            events.push(ev.event);
        }
        assert_eq!(
            events,
            vec![
                AgentHookEvent::PreToolUse {
                    tool: "Bash".into()
                },
                AgentHookEvent::Stop,
            ]
        );
        assert!(
            control_rx.try_recv().is_err(),
            "hook lines do not route to the control channel"
        );
    }

    #[tokio::test]
    async fn connection_skips_malformed_lines() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel(8);
        let input: &[u8] = b"not json\n{\"hook\":\"stop\"}\n";

        serve_connection(read_only(input), WorkspaceUid(1), &tx, &control_tx).await;
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.event);
        }
        assert_eq!(events, vec![AgentHookEvent::Stop]);
    }

    #[tokio::test]
    async fn open_editor_request_routes_to_control_and_replies_on_close() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let uid = WorkspaceUid(5);

        let (mut client, server) = tokio::io::duplex(256);
        client
            .write_all(b"{\"req\":\"open-editor\",\"path\":\"/tmp/msg\"}\n")
            .await
            .unwrap();

        let conn = tokio::spawn(async move {
            serve_connection(server, uid, &tx, &control_tx).await;
        });

        let AgentControl::OpenEditor {
            uid: got_uid,
            path,
            done,
        } = control_rx.recv().await.expect("control message")
        else {
            panic!("expected an open-editor control message");
        };
        assert_eq!(got_uid, uid);
        assert_eq!(path, PathBuf::from("/tmp/msg"));

        done.send(())
            .expect("connection still parked on the waiter");

        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "{\"reply\":\"editor-closed\"}\n");
        conn.await.unwrap();
    }

    #[tokio::test]
    async fn query_requests_route_to_control_and_reply_over_one_connection() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let uid = WorkspaceUid(9);

        let (client, server) = tokio::io::duplex(256);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut replies = BufReader::new(client_read).lines();

        let conn = tokio::spawn(async move {
            serve_connection(server, uid, &tx, &control_tx).await;
        });

        client_write
            .write_all(b"{\"req\":\"lsp-status\"}\n")
            .await
            .unwrap();
        let AgentControl::Query {
            uid: got_uid,
            request,
            reply,
        } = control_rx.recv().await.expect("first query")
        else {
            panic!("expected a query control message");
        };
        assert_eq!(got_uid, uid);
        assert_eq!(request, AgentQuery::LspStatus);
        reply.send(serde_json::json!({ "active": true })).unwrap();
        assert_eq!(
            replies.next_line().await.unwrap().unwrap(),
            r#"{"active":true}"#
        );

        client_write
            .write_all(b"{\"req\":\"diagnostics\"}\n")
            .await
            .unwrap();
        let AgentControl::Query { request, reply, .. } =
            control_rx.recv().await.expect("second query")
        else {
            panic!("expected a second query control message");
        };
        assert_eq!(request, AgentQuery::Diagnostics { path: None });
        reply.send(serde_json::json!([])).unwrap();
        assert_eq!(replies.next_line().await.unwrap().unwrap(), "[]");

        // Both split halves must drop for the duplex to close and the server's
        // read loop to see EOF and return.
        drop(client_write);
        drop(replies);
        conn.await.unwrap();
    }
}
