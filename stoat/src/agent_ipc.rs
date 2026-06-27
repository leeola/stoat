//! Per-session IPC server for Claude hook events.
//!
//! Each owned Claude subshell is spawned with `STOAT_AGENT_SOCK` pointing at a
//! per-session Unix socket (see [`crate::run::agent_socket_path`]). This module
//! binds that socket, reads newline-framed JSON hook events from connecting
//! clients, and forwards them to the render process's event loop as
//! [`AgentEvent`]s, which it applies to the owning workspace's
//! [`AgentStatus`](crate::agent_status::AgentStatus).

use crate::{agent_status::AgentHookEvent, workspace::WorkspaceUid};
use serde::Deserialize;
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
            match request {
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
                },
            }
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        } = control_rx.recv().await.expect("control message");
        assert_eq!(got_uid, uid);
        assert_eq!(path, PathBuf::from("/tmp/msg"));

        done.send(())
            .expect("connection still parked on the waiter");

        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "{\"reply\":\"editor-closed\"}\n");
        conn.await.unwrap();
    }
}
