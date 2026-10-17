//! Per-session IPC server for the shells this instance owns.
//!
//! Every owned Claude subshell and every terminal pane's shell is spawned with
//! `STOAT_AGENT_SOCK` pointing at a per-session Unix socket (see
//! [`crate::run::agent_socket_path`]). This module binds that socket and reads
//! newline-framed JSON from connecting clients.
//!
//! A hook line forwards to the render process's event loop as an
//! [`AgentEvent`], which it applies to the owning workspace's
//! [`AgentStatus`](crate::agent_status::AgentStatus). A request line instead
//! rides [`AgentControl`] and expects a reply. A query answers with live
//! session state. `open` opens files in front of the terminal pane the caller
//! runs in, which is what makes a `stoat <file>` inside a pane reach the
//! instance hosting it. With no terminal it opens them in a fresh split, as an
//! owned agent's `$EDITOR` asks, or in the focused pane. It holds the caller
//! until those files close when it asks to wait, and answers whether each
//! closed clean. A caller that goes away while it waits leaves no waiter
//! behind.

use crate::{
    agent_status::AgentHookEvent, app::Stoat, host::LanguageServerFeature, workspace::WorkspaceUid,
};
use futures::stream::{FuturesUnordered, StreamExt};
use lsp_types::{HoverParams, TextDocumentIdentifier, TextDocumentPositionParams};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use stoat_text::{Bias, Point};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    net::UnixListener,
    sync::{
        mpsc::{Sender, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};

/// The number the next connection takes.
///
/// One counter serves every session socket in the process, so a number names
/// one connection whichever workspace holds its waiters.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);

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

/// How a bridged buffer left the editor, which decides the exit status of the
/// command that waits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeOutcome {
    /// The buffer was clean when its tie ended, so the edit stands.
    Closed,
    /// The buffer still held unsaved edits when its tie ended, so the command
    /// that waits on it reports the edit abandoned.
    Abandoned,
}

/// A control request from an owned agent that expects a reply.
///
/// Unlike [`AgentEvent`], a control request carries a channel sender that the
/// event loop fires when the requested interaction finishes. A sender has no
/// serde form, so the request travels outside the [`AgentHookEvent`] path. The
/// event loop routes it by `uid` to the owning workspace.
pub enum AgentControl {
    /// Open `paths` in the session's workspace.
    ///
    /// With `term`, the files cover the split pane showing the terminal whose
    /// [`token`](crate::term_session::TermSession::token) it is, in front of the
    /// shell that asked. With no such terminal and `split`, they open in a fresh
    /// split beside the focused pane, as an agent's `$EDITOR` asks. Otherwise
    /// they open in the focused pane. Each path past the first takes a split of
    /// its own.
    ///
    /// `done` answers whether a waiter was registered. It fires on every path
    /// through the handler, since a caller parked on a reply that never comes
    /// hangs the user's shell. With `hold`, each opened buffer takes a clone,
    /// and the command returns when the last of them leaves the editor. A
    /// sender dropped with no outcome reads as closed.
    Open {
        uid: WorkspaceUid,
        /// The connection that asked, which a [`Self::ClientGone`] names when
        /// it ends while held.
        client: u64,
        term: Option<u64>,
        paths: Vec<PathBuf>,
        split: bool,
        hold: Option<UnboundedSender<BridgeOutcome>>,
        done: oneshot::Sender<bool>,
    },
    /// Connection `client` ended while it waited on buffers.
    ///
    /// The event loop drops the waiters the connection parked, and the buffers
    /// stay open with their edits.
    ClientGone { uid: WorkspaceUid, client: u64 },
    /// Answer a live-session [`AgentQuery`] and fire `reply` with the JSON
    /// result. The connection stays open afterward, so several queries ride one
    /// connection.
    Query {
        uid: WorkspaceUid,
        request: AgentQuery,
        reply: oneshot::Sender<Value>,
    },
}

/// A read-only interrogation of live session state, answered by the event loop.
///
/// Separate from the [`AgentRequest`] wire form so the control channel carries
/// only genuine queries. The `open` request is an interaction routed through
/// [`AgentControl::Open`] and never appears here.
#[derive(Debug, PartialEq)]
pub enum AgentQuery {
    /// LSP host liveness, plus each server's name, process id, and serialized
    /// capabilities.
    LspStatus,
    /// The session's name, root, and focused file, as
    /// [`crate::session_report::session_info`] reports them.
    SessionInfo,
    /// What has focus and what the focused editor shows, as
    /// [`crate::session_report::focus`] reports them, with the text on screen
    /// when `text` is set.
    Focus { text: bool },
    /// Every selection of the focused editor, as
    /// [`crate::session_report::selection`] reports them, with the covered
    /// bytes when `text` is set.
    Selection { text: bool },
    /// Every buffer of the workspace and where each one shows, as
    /// [`crate::session_report::buffers`] reports them.
    Buffers,
    /// The terminal size and every tab, pane, and dock of the workspace, as
    /// [`crate::session_report::layout`] reports them.
    Layout,
    /// Diagnostics for `path`, or for every tracked path when `None`.
    Diagnostics { path: Option<PathBuf> },
    /// Hover at a 1-based `line` and 1-based byte `col` within `path`, which
    /// the answer converts to the server's position encoding.
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
    /// `{"req":"open","paths":["/abs/a"],"term":N,"wait":true,"split":false}`.
    ///
    /// The paths are absolute, since the requesting shell's working directory
    /// is not the workspace root. `term` is the terminal's `STOAT_TERM_ID`, and
    /// it is absent outside a terminal pane. `wait` and `split` are false when
    /// absent, so the plain form opens in the focused pane and exits.
    Open {
        paths: Vec<PathBuf>,
        #[serde(default)]
        term: Option<u64>,
        #[serde(default)]
        wait: bool,
        #[serde(default)]
        split: bool,
    },
    /// `{"req":"lsp-status"}`.
    LspStatus,
    /// `{"req":"session-info"}`.
    SessionInfo,
    /// `{"req":"focus"}` or `{"req":"focus","text":true}`.
    Focus {
        #[serde(default)]
        text: bool,
    },
    /// `{"req":"selection"}` or `{"req":"selection","text":true}`.
    Selection {
        #[serde(default)]
        text: bool,
    },
    /// `{"req":"buffers"}`.
    Buffers,
    /// `{"req":"layout"}`.
    Layout,
    /// `{"req":"diagnostics"}` or `{"req":"diagnostics","path":"..."}`.
    Diagnostics { path: Option<PathBuf> },
    /// `{"req":"hover","path":"...","line":N,"col":N}`. `line` and `col` are a
    /// 1-based line and byte column, as every other request reports positions.
    Hover { path: PathBuf, line: u32, col: u32 },
}

/// The socket file a hook server bound, removed when the server stops.
///
/// The server stops when its accept loop ends or when its task is dropped,
/// which the session does as its run loop ends and the runtime does at exit.
/// The file goes either way. Two sessions that share a workspace uid bind the
/// same path, and the later bind replaces the file. The guard removes the path
/// only while it still names the file this server bound, so the exit of the
/// earlier session leaves the later one's socket.
struct BoundSocket {
    path: PathBuf,
    /// Device, inode, and change time of the bound file. A file system reuses a
    /// freed inode, so the inode alone does not tell a successor's socket from
    /// this one.
    identity: Option<(u64, u64, i64, i64)>,
}

impl BoundSocket {
    fn new(path: PathBuf) -> Self {
        let identity = Self::identity_of(&path);
        Self { path, identity }
    }

    fn identity_of(path: &Path) -> Option<(u64, u64, i64, i64)> {
        let meta = fs::symlink_metadata(path).ok()?;
        Some((meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
    }
}

// The socket file is the endpoint the listener bound, not the user-file IO that
// FsHost abstracts. No fake host holds a bound socket, so the guard removes the
// real file.
#[allow(clippy::disallowed_methods)]
impl Drop for BoundSocket {
    fn drop(&mut self) {
        if self.identity.is_some() && Self::identity_of(&self.path) == self.identity {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Bind the per-session socket at `socket_path` and forward decoded hook events
/// to `tx` and decoded control requests to `control_tx`, until the listener
/// fails or the receiver is dropped.
///
/// Spawned on the render process's executor. A stale socket file at the path is
/// removed before binding. Bind and accept failures are logged and stop the
/// server, leaving the app running without hook status for that session.
///
/// The server removes its socket file when it stops, which includes the drop
/// of its task. The session drops it as its run loop ends, and the runtime
/// drops it at exit. A file that a later session bound at the same path stays.
///
/// Connections are served side by side, so one parked on an open editor holds
/// up no other. A server that stops drops its parked connections with it.
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
    let _bound = BoundSocket::new(socket_path);

    // An empty set answers `None`, which turns its arm off for the round, so
    // the loop then waits on `accept` alone.
    let mut connections = FuturesUnordered::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.push(serve_connection(stream, uid, &tx, &control_tx));
                },
                Err(err) => {
                    tracing::warn!(%err, "agent hook server stopped accepting");
                    break;
                },
            },
            Some(()) = connections.next() => {},
        }
        if tx.is_closed() {
            break;
        }
    }
}

/// Forward one client connection's hook events to `tx` and its requests to
/// `control_tx`, writing each request's reply back over the same connection.
///
/// Each line is tried as an [`AgentRequest`] first, then as an
/// [`AgentHookEvent`]. An `open` request replies `opened` once the event loop
/// answers. When that answer holds buffers, the connection parks until the
/// event loop releases its waiters, then replies `closed` with the exit status
/// from [`held_status`], since the caller's `$EDITOR` blocks for exactly that
/// long. Every request replies and reads on, so one connection carries a
/// series.
///
/// A client that goes away while parked is reported as
/// [`AgentControl::ClientGone`], so its waiters do not outlive it.
///
/// Otherwise returns when the client disconnects, a read fails, or a receiver
/// is dropped. Blank lines are ignored and malformed lines are logged and
/// skipped, so one bad line never tears down the connection.
async fn serve_connection<R>(
    stream: R,
    uid: WorkspaceUid,
    tx: &Sender<AgentEvent>,
    control_tx: &Sender<AgentControl>,
) where
    R: AsyncRead + AsyncWrite + Unpin,
{
    let client = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
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
                AgentRequest::Open {
                    paths,
                    term,
                    wait,
                    split,
                } => {
                    let (done_tx, done_rx) = oneshot::channel();
                    let (hold_tx, mut hold_rx) = tokio::sync::mpsc::unbounded_channel();
                    if control_tx
                        .send(AgentControl::Open {
                            uid,
                            client,
                            term,
                            paths,
                            split,
                            hold: wait.then_some(hold_tx),
                            done: done_tx,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let held = done_rx.await.unwrap_or(false);
                    let opened = json!({ "reply": "opened", "held": held });
                    if !write_json_line(&mut write_half, &opened).await {
                        // The event loop parks the waiters before it answers,
                        // so a client that is gone before this reply holds
                        // them too.
                        if held {
                            report_client_gone(control_tx, uid, client).await;
                        }
                        return;
                    }
                    if held {
                        let Some(status) = held_status(&mut hold_rx, &mut lines).await else {
                            report_client_gone(control_tx, uid, client).await;
                            return;
                        };
                        let closed = json!({ "reply": "closed", "status": status });
                        if !write_json_line(&mut write_half, &closed).await {
                            return;
                        }
                    }
                    continue;
                },
                AgentRequest::LspStatus => AgentQuery::LspStatus,
                AgentRequest::SessionInfo => AgentQuery::SessionInfo,
                AgentRequest::Focus { text } => AgentQuery::Focus { text },
                AgentRequest::Selection { text } => AgentQuery::Selection { text },
                AgentRequest::Buffers => AgentQuery::Buffers,
                AgentRequest::Layout => AgentQuery::Layout,
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
            if !write_json_line(&mut write_half, &value).await {
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

/// The exit status for a command once every buffer it waits on has left the
/// editor, or `None` when the client goes away first.
///
/// A buffer left with unsaved edits makes it 1. A waiter dropped with no
/// outcome counts as closed. The end of `lines` or a failed read is a client
/// that went away. A line that arrives meanwhile is dropped, since no client
/// sends one while it waits.
async fn held_status<L>(
    hold: &mut UnboundedReceiver<BridgeOutcome>,
    lines: &mut Lines<L>,
) -> Option<i32>
where
    L: AsyncBufRead + Unpin,
{
    let mut status = 0;
    loop {
        // Both futures are cancel safe, so the arm that loses a round drops no
        // input.
        tokio::select! {
            outcome = hold.recv() => match outcome {
                Some(BridgeOutcome::Abandoned) => status = 1,
                Some(BridgeOutcome::Closed) => {},
                None => return Some(status),
            },
            line = lines.next_line() => {
                if !matches!(line, Ok(Some(_))) {
                    return None;
                }
            },
        }
    }
}

/// Tell the event loop that connection `client` went away while it waited on
/// buffers, so the waiters it parked go too.
async fn report_client_gone(control_tx: &Sender<AgentControl>, uid: WorkspaceUid, client: u64) {
    let _ = control_tx
        .send(AgentControl::ClientGone { uid, client })
        .await;
}

/// Write `value` to `out` as one JSON line, and answer whether the connection
/// still takes writes.
///
/// A value that does not encode is logged and skipped, which keeps the
/// connection open.
async fn write_json_line<W>(out: &mut W, value: &Value) -> bool
where
    W: AsyncWrite + Unpin,
{
    let mut encoded = match serde_json::to_vec(value) {
        Ok(encoded) => encoded,
        Err(err) => {
            tracing::warn!(%err, "failed to encode a reply");
            return true;
        },
    };
    encoded.push(b'\n');
    out.write_all(&encoded).await.is_ok()
}

/// Decode one newline-stripped JSON hook line into an [`AgentHookEvent`].
fn parse_hook_line(line: &str) -> Result<AgentHookEvent, serde_json::Error> {
    serde_json::from_str(line)
}

/// Answer a runtime [`AgentQuery`] from live session state, firing `reply` with
/// the JSON result.
///
/// `lsp-status`, `session-info`, `focus`, `selection`, `buffers`, `layout`,
/// and `diagnostics` reply synchronously. `hover` requires the path to be open
/// in the `uid` session (otherwise `{"error":"not open"}`) and runs the request
/// on a detached task so the event loop never blocks on the server. A zero
/// position or a line past the file's end replies an error before any request.
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
                    json!({ "name": name, "pid": host.pid(), "capabilities": capabilities })
                })
                .collect();
            let _ = reply.send(json!({
                "active": !servers.is_empty(),
                "spawn_attempted": stoat.lsp_registry.spawn_attempted_any(),
                "servers": servers,
            }));
        },
        AgentQuery::SessionInfo => {
            let _ = reply.send(crate::session_report::session_info(stoat, uid));
        },
        AgentQuery::Focus { text } => {
            let _ = reply.send(crate::session_report::focus(stoat, uid, text));
        },
        AgentQuery::Selection { text } => {
            let _ = reply.send(crate::session_report::selection(stoat, uid, text));
        },
        AgentQuery::Buffers => {
            let _ = reply.send(crate::session_report::buffers(stoat, uid));
        },
        AgentQuery::Layout => {
            let _ = reply.send(crate::session_report::layout(stoat, uid));
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
            let open = stoat
                .workspaces
                .iter()
                .find(|(_, ws)| ws.uid == uid)
                .and_then(|(_, ws)| {
                    let buffer_id = ws.buffers.id_for_path(&path)?;
                    let buffer = ws.buffers.get(buffer_id)?;
                    let rope = buffer.read().expect("buffer poisoned").rope().clone();
                    Some((buffer_id, rope))
                })
                .filter(|(buffer_id, _)| stoat.lsp_opened.contains(buffer_id));
            let Some((buffer_id, rope)) = open else {
                let _ = reply.send(json!({ "error": "not open" }));
                return;
            };
            let Some(uri) = crate::action_handlers::lsp::path_to_uri(&path) else {
                let _ = reply.send(json!({ "error": "invalid path" }));
                return;
            };
            if line == 0 || col == 0 {
                let _ = reply.send(json!({ "error": "lines and columns start at 1" }));
                return;
            }
            if line - 1 > rope.max_point().row {
                let _ = reply.send(json!({ "error": format!("no line {line}") }));
                return;
            }

            // A column past the line's end clamps to the end, and one inside a
            // character snaps back to the character's first byte.
            let offset = rope.clip_offset(
                rope.point_to_offset(Point::new(line - 1, col - 1)),
                Bias::Left,
            );
            let lsp =
                crate::lsp::hosts::lsp_for_feature(stoat, buffer_id, LanguageServerFeature::Hover);
            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: crate::lsp::util::byte_offset_to_lsp_pos(
                        &rope,
                        offset,
                        lsp.offset_encoding(),
                    ),
                },
                work_done_progress_params: Default::default(),
            };
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
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::{future::poll_fn, pin::pin, task::Poll, time::Duration};
    use tokio::{
        io::DuplexStream,
        net::UnixStream,
        sync::mpsc::{error::TryRecvError, Receiver},
        task::JoinHandle,
        time::Instant,
    };

    /// The `open` line an owned agent's `$EDITOR` sends.
    const AGENT_EDITOR_OPEN: &[u8] =
        b"{\"req\":\"open\",\"paths\":[\"/tmp/msg\"],\"wait\":true,\"split\":true}\n";

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
        let mut servers: Vec<(&str, &Value)> = value["servers"]
            .as_array()
            .expect("servers array")
            .iter()
            .map(|s| (s["name"].as_str().expect("server name"), &s["pid"]))
            .collect();
        servers.sort_by_key(|(name, _)| *name);
        assert_eq!(
            servers,
            [
                ("default", &Value::Null),
                ("primary", &Value::Null),
                ("secondary", &Value::Null),
            ],
            "the harness's sole fake and both installed fakes, none with a pid",
        );
    }

    /// Open a terminal in the focused pane and report its id and token.
    fn terminal_in_focused_pane(h: &mut TestHarness) -> (crate::term_session::TermId, u64) {
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace();
        let crate::pane::View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the terminal action should leave a terminal in the focused pane");
        };
        (term_id, ws.terms[term_id].token)
    }

    /// The file `pane` shows, or `None` when it shows anything but an editor.
    fn shown_path(h: &TestHarness, pane: crate::pane::PaneId) -> Option<PathBuf> {
        let ws = h.stoat.active_workspace();
        let crate::pane::View::Editor(editor_id) = ws.panes.pane(pane).view else {
            return None;
        };
        let buffer_id = ws.editors.get(editor_id)?.buffer_id;
        ws.buffers.path_for(buffer_id).map(|p| p.to_path_buf())
    }

    fn open_in_term(
        h: &mut TestHarness,
        term: u64,
        paths: Vec<PathBuf>,
    ) -> (crate::app::UpdateEffect, oneshot::Receiver<bool>) {
        request_open_in_term(h, term, paths, None)
    }

    /// Open `paths` as a request that waits, and return its answer and the
    /// receiver the held buffers report to.
    fn open_in_term_held(
        h: &mut TestHarness,
        term: u64,
        paths: Vec<PathBuf>,
    ) -> (oneshot::Receiver<bool>, UnboundedReceiver<BridgeOutcome>) {
        let (hold_tx, hold_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, done_rx) = request_open_in_term(h, term, paths, Some(hold_tx));
        (done_rx, hold_rx)
    }

    fn request_open_in_term(
        h: &mut TestHarness,
        term: u64,
        paths: Vec<PathBuf>,
        hold: Option<UnboundedSender<BridgeOutcome>>,
    ) -> (crate::app::UpdateEffect, oneshot::Receiver<bool>) {
        let uid = h.stoat.active_workspace().uid();
        let (done_tx, done_rx) = oneshot::channel();
        let effect = h.stoat.handle_agent_control(AgentControl::Open {
            uid,
            client: 0,
            term: Some(term),
            paths,
            split: false,
            hold,
            done: done_tx,
        });
        (effect, done_rx)
    }

    /// Open `paths` with no terminal named, as an agent's `$EDITOR` or a shell
    /// outside any pane asks.
    fn open_with_no_terminal(h: &mut TestHarness, paths: Vec<PathBuf>, split: bool) {
        let uid = h.stoat.active_workspace().uid();
        h.stoat.handle_agent_control(AgentControl::Open {
            uid,
            client: 0,
            term: None,
            paths,
            split,
            hold: None,
            done: oneshot::channel().0,
        });
    }

    #[test]
    fn an_open_with_no_terminal_and_split_opens_beside_the_focused_pane() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let before = h.stoat.active_workspace().panes.focus();

        open_with_no_terminal(&mut h, vec![root.join("a.rs")], true);

        let ws = h.stoat.active_workspace();
        let after = ws.panes.focus();
        assert_eq!(
            (
                ws.panes.split_pane_ids().len(),
                shown_path(&h, before),
                shown_path(&h, after),
            ),
            (2, None, Some(root.join("a.rs"))),
            "the scratch pane keeps its view and the file opens in a new split",
        );
    }

    #[test]
    fn an_open_with_no_terminal_and_no_split_uses_the_focused_pane() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let focus = h.stoat.active_workspace().panes.focus();

        open_with_no_terminal(&mut h, vec![root.join("a.rs")], false);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.panes.split_pane_ids().len(),
                ws.panes.focus(),
                shown_path(&h, focus),
            ),
            (1, focus, Some(root.join("a.rs"))),
        );
    }

    /// Split a labelled pane beside the focused one and leave focus on it, so a
    /// request landing on the terminal's pane is the token's doing rather than
    /// the fallback's.
    fn focus_a_pane_beside(h: &mut TestHarness) -> crate::pane::PaneId {
        let ws = h.stoat.active_workspace_mut();
        let side = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.pane_mut(side).view = crate::pane::View::Label("side".into());
        side
    }

    #[test]
    fn open_in_term_covers_the_named_terminal_and_keeps_it_reachable() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let (term_id, token) = terminal_in_focused_pane(&mut h);
        let term_pane = h.stoat.active_workspace().panes.focus();
        let side = focus_a_pane_beside(&mut h);

        let (effect, mut done_rx) = open_in_term(&mut h, token, vec![root.join("a.rs")]);

        assert_eq!(effect, crate::app::UpdateEffect::Redraw);
        assert_eq!(
            shown_path(&h, term_pane),
            Some(root.join("a.rs")),
            "the token names the pane, not the focus",
        );
        assert_eq!(shown_path(&h, side), None, "the focused pane is untouched");
        let ws = h.stoat.active_workspace();
        assert!(
            matches!(ws.panes.pane(term_pane).prev_view, Some(crate::pane::View::Terminal(t)) if t == term_id),
            "the live shell stays reachable behind the buffer",
        );
        assert!(
            ws.terms.contains_key(term_id),
            "covering a terminal does not end its session",
        );
        assert_eq!(
            ws.panes.focus(),
            term_pane,
            "focus follows the buffer that just opened",
        );
        assert_eq!(h.stoat.focused_mode(), "normal");
        done_rx.try_recv().expect("the caller is unparked");
    }

    #[test]
    fn write_quit_on_a_buffer_over_a_shell_returns_to_the_shell() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let (term_id, token) = terminal_in_focused_pane(&mut h);
        open_in_term(&mut h, token, vec![root.join("a.rs")]);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::WriteQuit);
        h.settle();

        let ws = h.stoat.active_workspace();
        let shell_back = matches!(
            ws.panes.pane(ws.panes.focus()).view,
            crate::pane::View::Terminal(t) if t == term_id
        );
        assert_eq!(
            (shell_back, h.stoat.quit_requested),
            (true, false),
            "the landed write returns the pane to its shell and keeps the app",
        );
    }

    #[test]
    fn open_in_term_with_an_unknown_token_uses_the_focused_pane() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let (term_id, token) = terminal_in_focused_pane(&mut h);
        let term_pane = h.stoat.active_workspace().panes.focus();
        let side = focus_a_pane_beside(&mut h);

        let (_, mut done_rx) = open_in_term(&mut h, token + 1, vec![root.join("a.rs")]);

        assert_eq!(shown_path(&h, side), Some(root.join("a.rs")));
        let ws = h.stoat.active_workspace();
        assert!(
            matches!(ws.panes.pane(term_pane).view, crate::pane::View::Terminal(t) if t == term_id),
            "an unresolved token leaves the terminal showing",
        );
        assert!(
            ws.panes.pane(side).prev_view.is_none(),
            "an unresolved token records no return view",
        );
        assert!(ws.terms.contains_key(term_id));
        done_rx.try_recv().expect("the caller is unparked");
    }

    #[test]
    fn open_in_term_splits_a_pane_per_extra_path() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let (_, token) = terminal_in_focused_pane(&mut h);
        let first = h.stoat.active_workspace().panes.focus();

        let (_, mut done_rx) =
            open_in_term(&mut h, token, vec![root.join("a.rs"), root.join("b.rs")]);

        let panes = h.stoat.active_workspace().panes.split_pane_ids();
        assert_eq!(panes.len(), 2, "the second path splits a pane of its own");
        let second = panes
            .into_iter()
            .find(|&id| id != first)
            .expect("split pane");
        assert_eq!(shown_path(&h, first), Some(root.join("a.rs")));
        assert_eq!(shown_path(&h, second), Some(root.join("b.rs")));
        done_rx.try_recv().expect("the caller is unparked");
    }

    /// How many split panes show terminal `term_id`.
    fn panes_showing_terminal(h: &TestHarness, term_id: crate::term_session::TermId) -> usize {
        let ws = h.stoat.active_workspace();
        ws.panes
            .split_pane_ids()
            .into_iter()
            .filter(|&id| {
                let view = &ws.panes.pane(id).view;
                matches!(view, crate::pane::View::Terminal(t) if *t == term_id)
            })
            .count()
    }

    /// A directory reads as an error, so neither path opens, and the split for
    /// the second keeps an empty editor rather than a second view of the shell.
    #[test]
    fn open_in_term_with_two_unopenable_paths_keeps_the_shell_in_one_pane() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("d/x.rs", ""), ("e/y.rs", "")]);
        let (term_id, token) = terminal_in_focused_pane(&mut h);

        let _ = open_in_term(&mut h, token, vec![root.join("d"), root.join("e")]);

        assert_eq!(panes_showing_terminal(&h, term_id), 1);
    }

    #[test]
    fn an_unopenable_later_path_gets_an_editor_of_its_own() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("d/x.rs", "")]);
        let (_, token) = terminal_in_focused_pane(&mut h);

        let _ = open_in_term(&mut h, token, vec![root.join("a.rs"), root.join("d")]);

        let ws = h.stoat.active_workspace();
        let editors: Vec<_> = ws
            .panes
            .split_pane_ids()
            .into_iter()
            .filter_map(|id| match ws.panes.pane(id).view {
                crate::pane::View::Editor(editor) => Some(editor),
                _ => None,
            })
            .collect();
        assert_eq!(editors.len(), 2, "both split panes show an editor");
        assert_ne!(editors[0], editors[1], "and not the same one");
    }

    #[test]
    fn a_failed_agent_open_leaves_no_second_view_of_the_focused_terminal() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("d/x.rs", "")]);
        let (term_id, _) = terminal_in_focused_pane(&mut h);
        let uid = h.stoat.active_workspace().uid();
        let (hold, _hold_rx) = tokio::sync::mpsc::unbounded_channel();

        let _ = h.stoat.handle_agent_control(AgentControl::Open {
            uid,
            client: 0,
            term: None,
            paths: vec![root.join("d")],
            split: true,
            hold: Some(hold),
            done: oneshot::channel().0,
        });

        assert_eq!(panes_showing_terminal(&h, term_id), 1);
    }

    #[test]
    fn a_deferred_open_in_term_keeps_the_next_keys_from_the_shell() {
        let mut h = TestHarness::with_size(80, 24);
        let path = seed_huge_file(&mut h);
        let (term_id, token) = terminal_in_focused_pane(&mut h);

        let (_, mut done_rx) = open_in_term(&mut h, token, vec![path]);

        let ws = h.stoat.active_workspace();
        assert!(
            matches!(ws.panes.pane(ws.panes.focus()).view, crate::pane::View::Terminal(t) if t == term_id),
            "a deferred open leaves the shell on screen",
        );
        h.stoat.update(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert!(
            h.fake_terminal().sent_bytes().is_empty(),
            "the next keys are the user's, not the shell's",
        );
        done_rx.try_recv().expect("the caller is unparked");
    }

    #[test]
    fn open_in_term_with_no_paths_still_unparks_the_caller() {
        let mut h = TestHarness::with_size(80, 24);
        let (_, token) = terminal_in_focused_pane(&mut h);

        let (effect, mut done_rx) = open_in_term(&mut h, token, Vec::new());

        assert_eq!(effect, crate::app::UpdateEffect::None);
        done_rx.try_recv().expect("the caller is unparked");
    }

    #[test]
    fn a_held_open_in_term_waits_for_every_buffer_it_opened() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let (term_id, token) = terminal_in_focused_pane(&mut h);
        let term_pane = h.stoat.active_workspace().panes.focus();

        let (mut done_rx, mut hold_rx) =
            open_in_term_held(&mut h, token, vec![root.join("a.rs"), root.join("b.rs")]);
        let opened = (done_rx.try_recv(), hold_rx.try_recv());

        let split = h.stoat.active_workspace().panes.focus();
        crate::action_handlers::close_pane_by_id(&mut h.stoat, split);
        let after_split = [hold_rx.try_recv(), hold_rx.try_recv()];

        h.stoat.active_workspace_mut().panes.set_focus(term_pane);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Quit);
        let after_quit = [hold_rx.try_recv(), hold_rx.try_recv()];

        let ws = h.stoat.active_workspace();
        let shell_back = matches!(
            ws.panes.pane(term_pane).view,
            crate::pane::View::Terminal(t) if t == term_id
        );
        assert_eq!(
            (opened, after_split, after_quit, shell_back),
            (
                (Ok(true), Err(TryRecvError::Empty)),
                [Ok(BridgeOutcome::Closed), Err(TryRecvError::Empty)],
                [Ok(BridgeOutcome::Closed), Err(TryRecvError::Disconnected)],
                true,
            ),
        );
    }

    #[test]
    fn a_held_open_in_term_that_opens_nothing_holds_nothing() {
        let mut h = TestHarness::with_size(80, 24);
        let path = seed_unreadable_file(&mut h);
        let (_, token) = terminal_in_focused_pane(&mut h);

        let (mut done_rx, mut hold_rx) = open_in_term_held(&mut h, token, vec![path]);

        assert_eq!(
            (done_rx.try_recv(), hold_rx.try_recv()),
            (Ok(false), Err(TryRecvError::Disconnected)),
        );
    }

    #[test]
    fn a_held_open_in_term_is_labelled_for_the_foreground_process() {
        for (foreground, label) in [(None, "shell"), (Some("git"), "git")] {
            let mut h = TestHarness::with_size(80, 24);
            let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
            if let Some(name) = foreground {
                h.fake_terminal().set_foreground_name(name);
            }
            let (_, token) = terminal_in_focused_pane(&mut h);

            let _parked = open_in_term_held(&mut h, token, vec![root.join("a.rs")]);
            let opened = crate::action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("the open covers the terminal's pane")
                .buffer_id;

            assert_eq!(
                h.stoat.active_workspace().held_buffer_labels(),
                [(opened, label.to_string())],
                "foreground process {foreground:?}",
            );
        }
    }

    /// A file that reads on the blocking pool holds the caller from the start.
    /// The buffer the read installs as releases the caller when it leaves the
    /// editor.
    #[test]
    fn a_held_open_in_term_waits_for_a_file_read_on_the_pool() {
        let mut h = TestHarness::with_size(80, 24);
        let path = seed_huge_file(&mut h);
        let (_, token) = terminal_in_focused_pane(&mut h);

        let (mut done_rx, mut hold_rx) = open_in_term_held(&mut h, token, vec![path]);
        let opened = (done_rx.try_recv(), hold_rx.try_recv());

        h.settle();
        crate::buffer_lifecycle::install_pending_opens(&mut h.stoat);
        let buffer = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("the read lands in the terminal's pane")
            .buffer_id;
        let labels = h.stoat.active_workspace().held_buffer_labels();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Quit);
        let after_quit = [hold_rx.try_recv(), hold_rx.try_recv()];

        assert_eq!(
            (opened, labels, after_quit),
            (
                (Ok(true), Err(TryRecvError::Empty)),
                vec![(buffer, "shell".to_string())],
                [Ok(BridgeOutcome::Closed), Err(TryRecvError::Disconnected)],
            ),
        );
    }

    /// A caller that goes away while its file still reads leaves no waiter on
    /// the buffer the read installs as.
    #[test]
    fn a_held_client_gone_before_its_read_lands_parks_nothing() {
        let mut h = TestHarness::with_size(80, 24);
        let path = seed_huge_file(&mut h);
        let (_, token) = terminal_in_focused_pane(&mut h);
        let term_pane = h.stoat.active_workspace().panes.focus();
        let (_done_rx, mut hold_rx) = open_in_term_held(&mut h, token, vec![path.clone()]);

        let uid = h.stoat.active_workspace().uid();
        h.stoat
            .handle_agent_control(AgentControl::ClientGone { uid, client: 0 });
        h.settle();
        crate::buffer_lifecycle::install_pending_opens(&mut h.stoat);

        assert_eq!(
            (
                shown_path(&h, term_pane),
                h.stoat.active_workspace().held_buffer_labels(),
                hold_rx.try_recv(),
            ),
            (Some(path), Vec::new(), Err(TryRecvError::Disconnected)),
        );
    }

    /// Seed a file past the inline-read ceiling, so its open lands on the pool
    /// and the pane still shows the shell when the handler returns.
    fn seed_huge_file(h: &mut TestHarness) -> PathBuf {
        let root = PathBuf::from("/big");
        let path = root.join("huge.txt");
        h.fake_fs().insert_file(&path, vec![b'x'; (1 << 20) + 16]);
        h.stoat.active_workspace_mut().git_root = root;
        path
    }

    /// Seed a small file whose bytes are not UTF-8, so its read fails and its
    /// open opens nothing.
    fn seed_unreadable_file(h: &mut TestHarness) -> PathBuf {
        let root = PathBuf::from("/latin1");
        let path = root.join("cafe.txt");
        h.fake_fs().insert_file(&path, b"caf\xe9\n");
        h.stoat.active_workspace_mut().git_root = root;
        path
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
                line: 1,
                col: 2,
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

    /// The fake speaks UTF-16, where the two-byte `\u{e9}` is one unit, so byte
    /// column 4 of the first line is the server's character 2.
    #[test]
    fn hover_converts_a_byte_column_to_the_servers_encoding() {
        use lsp_types::{Hover, HoverContents};

        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "\u{e9} abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_hover(path.to_str().unwrap(), 0, 2, "on a");

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path,
                line: 1,
                col: 4,
            },
            reply: reply_tx,
        });
        h.settle();

        let hover: Hover = serde_json::from_value(reply_rx.try_recv().expect("hover reply"))
            .expect("a hover, not an error");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover contents");
        };
        assert_eq!(markup.value, "on a");
    }

    #[test]
    fn hover_past_the_last_line_replies_an_error() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path,
                line: 3,
                col: 1,
            },
            reply: reply_tx,
        });

        assert_eq!(
            reply_rx.try_recv().expect("synchronous error reply"),
            serde_json::json!({ "error": "no line 3" }),
        );
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
                line: 1,
                col: 1,
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
        open_editor_replies(BridgeOutcome::Closed, 0).await;
    }

    #[tokio::test]
    async fn an_abandoned_editor_replies_with_status_one() {
        open_editor_replies(BridgeOutcome::Abandoned, 1).await;
    }

    /// Route the agent editor's `open` request, release its waiter with
    /// `outcome`, and assert the connection answers `opened` and then `closed`
    /// with `status`.
    async fn open_editor_replies(outcome: BridgeOutcome, status: i32) {
        let uid = WorkspaceUid(5);
        let (client, mut controls, conn) = serve_duplex(uid);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut replies = BufReader::new(client_read).lines();
        client_write.write_all(AGENT_EDITOR_OPEN).await.unwrap();

        let Some(AgentControl::Open {
            uid: got_uid,
            term,
            paths,
            split,
            hold,
            done,
            ..
        }) = controls.recv().await
        else {
            panic!("expected an open control message");
        };
        assert_eq!(
            (got_uid, term, paths, split),
            (uid, None, vec![PathBuf::from("/tmp/msg")], true),
        );
        let hold = hold.expect("a request that waits carries a hold");
        done.send(true).expect("connection parked on the open");
        let opened = next_reply(&mut replies).await;

        hold.send(outcome).expect("connection parked on the hold");
        drop(hold);
        let closed = next_reply(&mut replies).await;

        assert_eq!(
            [opened, closed],
            [
                json!({ "reply": "opened", "held": true }),
                json!({ "reply": "closed", "status": status }),
            ],
        );
        drop(client_write);
        drop(replies);
        conn.await.unwrap();
    }

    #[test]
    fn open_in_term_decodes_its_token_and_paths() {
        let decoded: AgentRequest =
            serde_json::from_str(r#"{"req":"open","term":7,"paths":["/abs/a","/abs/b"]}"#)
                .expect("an open decodes");
        let AgentRequest::Open {
            term,
            paths,
            wait,
            split,
        } = decoded
        else {
            panic!("expected an open request, got {decoded:?}");
        };
        assert_eq!(term, Some(7));
        assert_eq!(
            paths,
            vec![PathBuf::from("/abs/a"), PathBuf::from("/abs/b")]
        );
        assert!(
            !wait && !split,
            "a request with no wait or split field does neither"
        );

        let waiting: AgentRequest =
            serde_json::from_str(r#"{"req":"open","term":7,"paths":[],"wait":true}"#)
                .expect("a waiting open decodes");
        assert!(matches!(waiting, AgentRequest::Open { wait: true, .. }));
    }

    #[tokio::test]
    async fn open_in_term_replies_opened_and_keeps_reading() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let uid = WorkspaceUid(11);

        let (client, server) = tokio::io::duplex(256);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut replies = BufReader::new(client_read).lines();

        let conn = tokio::spawn(async move {
            serve_connection(server, uid, &tx, &control_tx).await;
        });

        client_write
            .write_all(b"{\"req\":\"open\",\"term\":3,\"paths\":[\"/abs/a\"]}\n")
            .await
            .unwrap();
        let AgentControl::Open {
            uid: got_uid,
            term,
            paths,
            hold,
            done,
            ..
        } = control_rx.recv().await.expect("control message")
        else {
            panic!("expected an open control message");
        };
        assert_eq!(got_uid, uid);
        assert_eq!(term, Some(3));
        assert_eq!(paths, vec![PathBuf::from("/abs/a")]);
        assert!(hold.is_none(), "a request with no wait holds nothing");
        done.send(false).expect("connection parked on the waiter");
        assert_eq!(
            next_reply(&mut replies).await,
            json!({ "reply": "opened", "held": false })
        );

        // A second request over the same connection proves the read loop goes
        // on after a reply.
        client_write
            .write_all(b"{\"req\":\"open\",\"term\":4,\"paths\":[\"/abs/b\"]}\n")
            .await
            .unwrap();
        let AgentControl::Open { term, done, .. } =
            control_rx.recv().await.expect("second control message")
        else {
            panic!("expected a second open control message");
        };
        assert_eq!(term, Some(4));
        done.send(false).expect("connection parked again");
        assert_eq!(
            next_reply(&mut replies).await,
            json!({ "reply": "opened", "held": false })
        );

        drop(client_write);
        drop(replies);
        conn.await.unwrap();
    }

    #[tokio::test]
    async fn a_held_open_in_term_replies_closed_with_its_status() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(8);
        let (client, server) = tokio::io::duplex(256);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut replies = BufReader::new(client_read).lines();
        let conn = tokio::spawn(async move {
            serve_connection(server, WorkspaceUid(12), &tx, &control_tx).await;
        });

        client_write
            .write_all(b"{\"req\":\"open\",\"term\":3,\"paths\":[\"/abs/a\"],\"wait\":true}\n")
            .await
            .unwrap();
        let AgentControl::Open { hold, done, .. } =
            control_rx.recv().await.expect("control message")
        else {
            panic!("expected an open control message");
        };
        let hold = hold.expect("a request that waits carries a hold");
        done.send(true).expect("connection parked on the open");
        let opened = next_reply(&mut replies).await;

        hold.send(BridgeOutcome::Abandoned)
            .expect("connection parked on the hold");
        drop(hold);
        let closed = next_reply(&mut replies).await;

        assert_eq!(
            [opened, closed],
            [
                json!({ "reply": "opened", "held": true }),
                json!({ "reply": "closed", "status": 1 }),
            ],
        );
        drop(client_write);
        drop(replies);
        conn.await.unwrap();
    }

    /// The next reply line on `replies`, parsed, so the assertion does not
    /// depend on the order the encoder writes the keys in.
    async fn next_reply<R>(replies: &mut Lines<R>) -> Value
    where
        R: AsyncBufRead + Unpin,
    {
        let line = replies
            .next_line()
            .await
            .expect("read a reply")
            .expect("a reply line");
        serde_json::from_str(&line).expect("a reply is JSON")
    }

    #[tokio::test]
    async fn an_editor_client_that_leaves_while_parked_is_reported_gone() {
        let uid = WorkspaceUid(13);
        let (client, mut controls, conn) = serve_duplex(uid);
        let mut client = BufReader::new(client);
        client.get_mut().write_all(AGENT_EDITOR_OPEN).await.unwrap();
        let Some(AgentControl::Open {
            client: parked,
            hold: _waiter,
            done,
            ..
        }) = controls.recv().await
        else {
            panic!("expected an open control message");
        };
        done.send(true).expect("connection parked on the open");
        client.read_line(&mut String::new()).await.unwrap();

        drop(client);

        assert_eq!(next_gone(&mut controls).await, (uid, parked));
        conn.await.unwrap();
    }

    #[tokio::test]
    async fn a_held_client_that_leaves_is_reported_gone() {
        for leaves_before_opened in [false, true] {
            let uid = WorkspaceUid(14);
            let (client, mut controls, conn) = serve_duplex(uid);
            let mut client = BufReader::new(client);
            client
                .get_mut()
                .write_all(b"{\"req\":\"open\",\"term\":3,\"paths\":[\"/abs/a\"],\"wait\":true}\n")
                .await
                .unwrap();
            let Some(AgentControl::Open {
                client: parked,
                hold: _waiter,
                done,
                ..
            }) = controls.recv().await
            else {
                panic!("expected an open control message");
            };

            if leaves_before_opened {
                drop(client);
                done.send(true).expect("connection parked on the open");
            } else {
                done.send(true).expect("connection parked on the open");
                client.read_line(&mut String::new()).await.unwrap();
                drop(client);
            }

            assert_eq!(
                next_gone(&mut controls).await,
                (uid, parked),
                "leaves before its opened reply: {leaves_before_opened}",
            );
            conn.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_stray_line_keeps_a_client_parked() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut lines = BufReader::new(server).lines();
        let (hold_tx, mut hold_rx) = tokio::sync::mpsc::unbounded_channel();
        client.write_all(b"stray\n").await.unwrap();

        let mut parked = pin!(held_status(&mut hold_rx, &mut lines));
        let first = poll_fn(|cx| Poll::Ready(parked.as_mut().poll(cx))).await;
        assert_eq!(first, Poll::Pending, "a stray line ends no wait");

        hold_tx.send(BridgeOutcome::Abandoned).unwrap();
        drop(hold_tx);
        assert_eq!(parked.await, Some(1));
    }

    /// Serve one in-memory connection for `uid` on a task, and return the
    /// client end with the receiver its control messages arrive on.
    fn serve_duplex(uid: WorkspaceUid) -> (DuplexStream, Receiver<AgentControl>, JoinHandle<()>) {
        let (tx, _) = tokio::sync::mpsc::channel(8);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(8);
        let (client, server) = tokio::io::duplex(256);
        let conn = tokio::spawn(async move {
            serve_connection(server, uid, &tx, &control_tx).await;
        });
        (client, control_rx, conn)
    }

    /// The session and the connection that the next control message reports
    /// gone.
    async fn next_gone(controls: &mut Receiver<AgentControl>) -> (WorkspaceUid, u64) {
        match controls.recv().await {
            Some(AgentControl::ClientGone { uid, client }) => (uid, client),
            _ => panic!("expected a client-gone control message"),
        }
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

    #[tokio::test]
    async fn a_stopped_server_removes_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let (server, _events, _controls) = serve_at(&path).await;

        server.abort();
        let _ = server.await;

        assert!(!path.exists(), "the socket outlives its server");
    }

    #[tokio::test]
    async fn a_stopped_server_keeps_a_socket_bound_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let (server, _events, _controls) = serve_at(&path).await;

        // A rename keeps the served file's inode in use, so the successor's
        // socket gets a different inode.
        tokio::fs::rename(&path, dir.path().join("displaced.sock"))
            .await
            .unwrap();
        let _successor = UnixListener::bind(&path).unwrap();
        server.abort();
        let _ = server.await;

        assert!(
            path.exists(),
            "the stopped server removed its successor's socket"
        );
    }

    #[tokio::test]
    async fn a_parked_editor_request_does_not_hold_up_a_second_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let (server, _events, mut controls) = serve_at(&path).await;

        let mut editor = UnixStream::connect(&path).await.unwrap();
        editor.write_all(AGENT_EDITOR_OPEN).await.unwrap();
        // Held to the end of the test, so its waiter never fires and the first
        // connection stays parked.
        let parked = controls.recv().await;
        assert!(matches!(parked, Some(AgentControl::Open { .. })));

        let mut query = UnixStream::connect(&path).await.unwrap();
        query
            .write_all(b"{\"req\":\"lsp-status\"}\n")
            .await
            .unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), controls.recv())
            .await
            .expect("a parked editor request holds up no other connection");
        assert!(matches!(
            next,
            Some(AgentControl::Query {
                request: AgentQuery::LspStatus,
                ..
            })
        ));

        server.abort();
        let _ = server.await;
    }

    /// Serve hooks at `path` on a task, and wait until the server binds it.
    ///
    /// The receivers come back with the task, because a server stops after its
    /// next connection once its event receiver is gone.
    async fn serve_at(
        path: &Path,
    ) -> (JoinHandle<()>, Receiver<AgentEvent>, Receiver<AgentControl>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(8);
        let server = tokio::spawn(serve_agent_hooks(
            path.to_path_buf(),
            WorkspaceUid(1),
            tx,
            control_tx,
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(Instant::now() < deadline, "the server never bound {path:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        (server, rx, control_rx)
    }
}
