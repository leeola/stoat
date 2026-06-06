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
}

impl ShellHandle {
    pub(crate) fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self { session }
    }

    pub fn send_command(&self, command: &str) {
        use futures::FutureExt;
        let payload = format!("{command}\n");
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
            ("PS0".into(), "\x1b]133;C\x07".into()),
            (
                "PROMPT_COMMAND".into(),
                "printf '\\033]133;D;%s\\007' \"$?\"".into(),
            ),
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

async fn reader_task(
    session: Arc<dyn TerminalSession>,
    run_id: RunId,
    tx: mpsc::Sender<PtyNotification>,
) {
    let mut buf = [0u8; 4096];

    loop {
        let n = match session.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        if tx
            .send(PtyNotification::Output {
                run_id,
                data: buf[..n].to_vec(),
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
