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
//! session state. `open-editor` parks the caller until its buffer closes and
//! answers whether the buffer closed clean. `open-in-term` opens files in the
//! terminal pane the caller runs in, which is what makes a `stoat <file>`
//! inside a pane reach the instance hosting it, and holds the caller until
//! those files close when it asks to wait.

use crate::{
    agent_status::AgentHookEvent, app::Stoat, host::LanguageServerFeature, workspace::WorkspaceUid,
};
use futures::stream::{FuturesUnordered, StreamExt};
use lsp_types::{HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{
        mpsc::{Sender, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
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
    /// Open `path` as a buffer in the session's workspace and keep the agent
    /// blocked until that buffer leaves the editor.
    ///
    /// The release sends a [`BridgeOutcome`] on `done` and drops it, which
    /// unblocks the parked socket connection so the agent's `$EDITOR`
    /// invocation returns. A sender dropped with no outcome reads as closed.
    OpenEditor {
        uid: WorkspaceUid,
        path: PathBuf,
        done: UnboundedSender<BridgeOutcome>,
    },
    /// Open `paths` in the split pane showing the terminal whose
    /// [`token`](crate::term_session::TermSession::token) is `term`, in front of
    /// the shell that asked.
    ///
    /// `done` answers whether a waiter was registered. It fires on every path
    /// through the handler, since a caller parked on a reply that never comes
    /// hangs the user's shell. With `hold`, each opened buffer takes a clone,
    /// and the command returns when the last of them leaves the editor.
    OpenInTerm {
        uid: WorkspaceUid,
        term: u64,
        paths: Vec<PathBuf>,
        hold: Option<UnboundedSender<BridgeOutcome>>,
        done: oneshot::Sender<bool>,
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
    /// LSP host liveness, plus each server's name, process id, and serialized
    /// capabilities.
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
    /// `{"req":"open-in-term","term":N,"paths":["/abs/a"],"wait":true}`.
    /// `term` is the terminal's `STOAT_TERM_ID`, and the paths are absolute,
    /// since the requesting shell's working directory is not the workspace
    /// root. `wait` is false when absent, which is the open-and-exit form.
    OpenInTerm {
        term: u64,
        paths: Vec<PathBuf>,
        #[serde(default)]
        wait: bool,
    },
    /// `{"req":"lsp-status"}`.
    LspStatus,
    /// `{"req":"diagnostics"}` or `{"req":"diagnostics","path":"..."}`.
    Diagnostics { path: Option<PathBuf> },
    /// `{"req":"hover","path":"...","line":N,"col":N}`. `line`/`col` are LSP
    /// UTF-16 positions, forwarded to the server unconverted.
    Hover { path: PathBuf, line: u32, col: u32 },
}

/// The socket file a hook server bound, removed when the server stops.
///
/// The server stops when its accept loop ends or when the runtime drops its
/// task at exit, and the file goes either way. Two sessions that share a
/// workspace uid bind the same path, and the later bind replaces the file. The
/// guard removes the path only while it still names the file this server
/// bound, so the exit of the earlier session leaves the later one's socket.
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
/// of its task when the runtime shuts down at exit. A file that a later session
/// bound at the same path stays.
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
/// [`AgentHookEvent`]. An open-editor request parks the connection until the
/// event loop releases its waiter, then writes an `editor-closed` reply with
/// the exit status from [`held_status`] and returns, since the caller's
/// `$EDITOR` blocks for exactly that long. Every other request replies and
/// reads on, so one connection carries a series.
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
                    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
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
                    let status = held_status(&mut done_rx).await;
                    let reply = json!({ "reply": "editor-closed", "status": status });
                    let _ = write_json_line(&mut write_half, &reply).await;
                    return;
                },
                AgentRequest::OpenInTerm { term, paths, wait } => {
                    let (done_tx, done_rx) = oneshot::channel();
                    let (hold_tx, mut hold_rx) = tokio::sync::mpsc::unbounded_channel();
                    if control_tx
                        .send(AgentControl::OpenInTerm {
                            uid,
                            term,
                            paths,
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
                        return;
                    }
                    if held {
                        let status = held_status(&mut hold_rx).await;
                        let closed = json!({ "reply": "closed", "status": status });
                        if !write_json_line(&mut write_half, &closed).await {
                            return;
                        }
                    }
                    continue;
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
/// editor.
///
/// A buffer left with unsaved edits makes it 1. A waiter dropped with no
/// outcome counts as closed.
async fn held_status(hold: &mut UnboundedReceiver<BridgeOutcome>) -> i32 {
    let mut status = 0;
    while let Some(outcome) = hold.recv().await {
        if outcome == BridgeOutcome::Abandoned {
            status = 1;
        }
    }
    status
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
                    json!({ "name": name, "pid": host.pid(), "capabilities": capabilities })
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
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;
    use tokio::{
        net::UnixStream,
        sync::mpsc::{error::TryRecvError, Receiver},
        task::JoinHandle,
        time::Instant,
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
        let effect = h.stoat.handle_agent_control(AgentControl::OpenInTerm {
            uid,
            term,
            paths,
            hold,
            done: done_tx,
        });
        (effect, done_rx)
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
        let path = seed_huge_file(&mut h);
        let (_, token) = terminal_in_focused_pane(&mut h);

        let (mut done_rx, mut hold_rx) = open_in_term_held(&mut h, token, vec![path]);

        assert_eq!(
            (done_rx.try_recv(), hold_rx.try_recv()),
            (Ok(false), Err(TryRecvError::Disconnected)),
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
        open_editor_replies(
            BridgeOutcome::Closed,
            "{\"reply\":\"editor-closed\",\"status\":0}\n",
        )
        .await;
    }

    #[tokio::test]
    async fn an_abandoned_editor_replies_with_status_one() {
        open_editor_replies(
            BridgeOutcome::Abandoned,
            "{\"reply\":\"editor-closed\",\"status\":1}\n",
        )
        .await;
    }

    /// Route an open-editor request, release its waiter with `outcome`, and
    /// assert the connection answers `expected`.
    async fn open_editor_replies(outcome: BridgeOutcome, expected: &str) {
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

        done.send(outcome)
            .expect("connection still parked on the waiter");
        drop(done);

        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, expected);
        conn.await.unwrap();
    }

    #[test]
    fn open_in_term_decodes_its_token_and_paths() {
        let decoded: AgentRequest =
            serde_json::from_str(r#"{"req":"open-in-term","term":7,"paths":["/abs/a","/abs/b"]}"#)
                .expect("open-in-term decodes");
        let AgentRequest::OpenInTerm { term, paths, wait } = decoded else {
            panic!("expected an open-in-term request, got {decoded:?}");
        };
        assert_eq!(term, 7);
        assert_eq!(
            paths,
            vec![PathBuf::from("/abs/a"), PathBuf::from("/abs/b")]
        );
        assert!(!wait, "a request with no wait field does not wait");

        let waiting: AgentRequest =
            serde_json::from_str(r#"{"req":"open-in-term","term":7,"paths":[],"wait":true}"#)
                .expect("a waiting open-in-term decodes");
        assert!(matches!(
            waiting,
            AgentRequest::OpenInTerm { wait: true, .. }
        ));
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
            .write_all(b"{\"req\":\"open-in-term\",\"term\":3,\"paths\":[\"/abs/a\"]}\n")
            .await
            .unwrap();
        let AgentControl::OpenInTerm {
            uid: got_uid,
            term,
            paths,
            hold,
            done,
        } = control_rx.recv().await.expect("control message")
        else {
            panic!("expected an open-in-term control message");
        };
        assert_eq!(got_uid, uid);
        assert_eq!(term, 3);
        assert_eq!(paths, vec![PathBuf::from("/abs/a")]);
        assert!(hold.is_none(), "a request with no wait holds nothing");
        done.send(false).expect("connection parked on the waiter");
        assert_eq!(
            next_reply(&mut replies).await,
            json!({ "reply": "opened", "held": false })
        );

        // A second request over the same connection proves the read loop
        // continued rather than returning like open-editor.
        client_write
            .write_all(b"{\"req\":\"open-in-term\",\"term\":4,\"paths\":[\"/abs/b\"]}\n")
            .await
            .unwrap();
        let AgentControl::OpenInTerm { term, done, .. } =
            control_rx.recv().await.expect("second control message")
        else {
            panic!("expected a second open-in-term control message");
        };
        assert_eq!(term, 4);
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
            .write_all(
                b"{\"req\":\"open-in-term\",\"term\":3,\"paths\":[\"/abs/a\"],\"wait\":true}\n",
            )
            .await
            .unwrap();
        let AgentControl::OpenInTerm { hold, done, .. } =
            control_rx.recv().await.expect("control message")
        else {
            panic!("expected an open-in-term control message");
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
    async fn next_reply<R>(replies: &mut tokio::io::Lines<R>) -> Value
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let line = replies
            .next_line()
            .await
            .expect("read a reply")
            .expect("a reply line");
        serde_json::from_str(&line).expect("a reply is JSON")
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
        editor
            .write_all(b"{\"req\":\"open-editor\",\"path\":\"/tmp/msg\"}\n")
            .await
            .unwrap();
        // Held to the end of the test, so its waiter never fires and the first
        // connection stays parked.
        let parked = controls.recv().await;
        assert!(matches!(parked, Some(AgentControl::OpenEditor { .. })));

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
