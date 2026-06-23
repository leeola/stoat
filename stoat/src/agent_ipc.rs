//! Per-session IPC server for Claude hook events.
//!
//! Each owned Claude subshell is spawned with `STOAT_AGENT_SOCK` pointing at a
//! per-session Unix socket (see [`crate::run::agent_socket_path`]). This module
//! binds that socket, reads newline-framed JSON hook events from connecting
//! clients, and forwards them to the render process's event loop as
//! [`AgentEvent`]s, which it applies to the owning workspace's
//! [`AgentStatus`](crate::agent_status::AgentStatus).

use crate::{agent_status::AgentHookEvent, workspace::WorkspaceUid};
use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    net::UnixListener,
    sync::mpsc::Sender,
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

/// Bind the per-session hook socket at `socket_path` and forward decoded events
/// to `tx` until the listener fails or the receiver is dropped.
///
/// Spawned on the render process's executor. A stale socket file at the path is
/// removed before binding. Bind and accept failures are logged and stop the
/// server, leaving the app running without hook status for that session.
pub async fn serve_agent_hooks(socket_path: PathBuf, uid: WorkspaceUid, tx: Sender<AgentEvent>) {
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
            Ok((stream, _)) => serve_connection(stream, uid, &tx).await,
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

/// Read newline-framed JSON hook events from one client connection and forward
/// each as an [`AgentEvent`].
///
/// Returns when the client disconnects, a read fails, or the receiver is
/// dropped. Blank lines are ignored and malformed lines are logged and skipped,
/// so one bad line never tears down the connection.
async fn serve_connection<R>(stream: R, uid: WorkspaceUid, tx: &Sender<AgentEvent>)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
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

    #[tokio::test]
    async fn connection_forwards_each_hook_line() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let uid = WorkspaceUid(7);
        let input: &[u8] =
            b"{\"hook\":\"pre-tool-use\",\"tool\":\"Bash\"}\n\n{\"hook\":\"stop\"}\n";

        serve_connection(input, uid, &tx).await;
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
    }

    #[tokio::test]
    async fn connection_skips_malformed_lines() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let input: &[u8] = b"not json\n{\"hook\":\"stop\"}\n";

        serve_connection(input, WorkspaceUid(1), &tx).await;
        drop(tx);

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.event);
        }
        assert_eq!(events, vec![AgentHookEvent::Stop]);
    }
}
