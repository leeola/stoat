use super::RunId;
use crate::host::terminal::{open_local_pty, SpawnArgs, TerminalSession};
use std::{path::Path, sync::Arc};
use stoat_scheduler::Executor;
use tokio::sync::mpsc;

pub enum PtyNotification {
    Output {
        run_id: RunId,
        data: Vec<u8>,
    },
    CommandDone {
        run_id: RunId,
        exit_status: Option<i32>,
    },
}

pub struct ShellHandle {
    session: Arc<dyn TerminalSession>,
    pub active_sentinel: Option<String>,
}

impl ShellHandle {
    pub(crate) fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self {
            session,
            active_sentinel: None,
        }
    }

    pub fn send_command(&self, command: &str, sentinel: &str) {
        use futures::FutureExt;
        let payload = format!("{command}\necho {sentinel} $?\n");
        let _ = self.session.write(payload.as_bytes()).now_or_never();
    }

    pub fn send_interrupt(&self) {
        use futures::FutureExt;
        let _ = self.session.write(b"\x03").now_or_never();
    }

    pub fn kill(&self) {
        use futures::FutureExt;
        let _ = self.session.kill().now_or_never();
    }
}

pub fn spawn_shell(
    executor: &Executor,
    cwd: &Path,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["--noediting".into(), "--noprofile".into(), "--norc".into()],
        env: vec![
            ("PS1".into(), String::new()),
            ("PS2".into(), String::new()),
            ("TERM".into(), "dumb".into()),
        ],
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    let session: Arc<dyn TerminalSession> = Arc::new(open_local_pty(args)?);
    executor
        .spawn(reader_task(session.clone(), run_id, pty_tx))
        .detach();

    Ok(ShellHandle::new(session))
}

pub fn spawn_oneshot(
    executor: &Executor,
    command: &str,
    cwd: &Path,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["-c".into(), command.to_string()],
        env: vec![("TERM".into(), "dumb".into())],
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    let session: Arc<dyn TerminalSession> = Arc::new(open_local_pty(args)?);
    executor
        .spawn(reader_task(session.clone(), run_id, pty_tx))
        .detach();

    Ok(ShellHandle::new(session))
}

async fn reader_task(
    session: Arc<dyn TerminalSession>,
    run_id: RunId,
    tx: mpsc::Sender<PtyNotification>,
) {
    let mut line_buf = String::new();

    loop {
        let chunk = match session.read_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };

        let n = chunk.len();
        let mut output_start = 0;

        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'\n' || i == n - 1 {
                let end = if byte == b'\n' { i } else { i + 1 };
                if let Ok(segment) = std::str::from_utf8(&chunk[output_start..end]) {
                    line_buf.push_str(segment.trim_end_matches('\r'));
                }

                if line_buf.starts_with("__STOAT_")
                    && line_buf.contains("__")
                    && let Some(status) = parse_sentinel_line(&line_buf)
                {
                    let _ = tx
                        .send(PtyNotification::CommandDone {
                            run_id,
                            exit_status: Some(status),
                        })
                        .await;
                    line_buf.clear();
                    output_start = end + 1;
                    continue;
                }

                line_buf.clear();
                output_start = end + 1;
            }
        }

        if tx
            .send(PtyNotification::Output {
                run_id,
                data: chunk,
            })
            .await
            .is_err()
        {
            break;
        }
    }

    let _ = tx
        .send(PtyNotification::CommandDone {
            run_id,
            exit_status: None,
        })
        .await;
}

pub(super) fn parse_sentinel_line(line: &str) -> Option<i32> {
    let rest = line.strip_prefix("__STOAT_")?;
    let after_id = rest.find("__ ")?;
    let status_str = &rest[after_id + 3..];
    status_str.trim().parse().ok()
}
