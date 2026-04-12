use super::RunId;
use crate::host::terminal::{PtyTerminal, TerminalHost};
use portable_pty::CommandBuilder;
use std::path::PathBuf;
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
    host: Box<dyn TerminalHost>,
    pub active_sentinel: Option<String>,
}

impl ShellHandle {
    pub(crate) fn new(host: Box<dyn TerminalHost>) -> Self {
        Self {
            host,
            active_sentinel: None,
        }
    }

    pub fn send_command(&mut self, command: &str, sentinel: &str) {
        let payload = format!("{command}\necho {sentinel} $?\n");
        let _ = self.host.write(payload.as_bytes());
        self.active_sentinel = Some(sentinel.to_owned());
    }

    pub fn send_interrupt(&mut self) {
        let _ = self.host.write(b"\x03");
    }

    pub fn kill(&mut self) {
        let _ = self.host.kill();
    }
}

pub fn spawn_shell(
    cwd: &PathBuf,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: width,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["--noediting", "--noprofile", "--norc"]);
    cmd.env("PS1", "");
    cmd.env("PS2", "");
    cmd.env("TERM", "dumb");
    cmd.cwd(cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    tokio::task::spawn_blocking(move || {
        pty_reader_task(reader, pty_tx, run_id);
    });

    let terminal = PtyTerminal::new(writer, child);
    Ok(ShellHandle::new(Box::new(terminal)))
}

pub fn spawn_oneshot(
    command: &str,
    cwd: &PathBuf,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: width,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["-c", command]);
    cmd.env("TERM", "dumb");
    cmd.cwd(cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    tokio::task::spawn_blocking(move || {
        pty_reader_task(reader, pty_tx, run_id);
    });

    let terminal = PtyTerminal::new(writer, child);
    Ok(ShellHandle::new(Box::new(terminal)))
}

fn pty_reader_task(
    mut reader: Box<dyn std::io::Read + Send>,
    tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    let mut line_buf = String::new();

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        let chunk = &buf[..n];
        let mut output_start = 0;

        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'\n' || i == n - 1 {
                let end = if byte == b'\n' { i } else { i + 1 };
                if let Ok(segment) = std::str::from_utf8(&chunk[output_start..end]) {
                    line_buf.push_str(segment.trim_end_matches('\r'));
                }

                if line_buf.starts_with("__STOAT_") && line_buf.contains("__") {
                    if let Some(status) = parse_sentinel_line(&line_buf) {
                        let _ = tx.blocking_send(PtyNotification::CommandDone {
                            run_id,
                            exit_status: Some(status),
                        });
                        line_buf.clear();
                        output_start = end + 1;
                        continue;
                    }
                }

                line_buf.clear();
                output_start = end + 1;
            }
        }

        if tx
            .blocking_send(PtyNotification::Output {
                run_id,
                data: chunk.to_vec(),
            })
            .is_err()
        {
            break;
        }
    }

    let _ = tx.blocking_send(PtyNotification::CommandDone {
        run_id,
        exit_status: None,
    });
}

pub(super) fn parse_sentinel_line(line: &str) -> Option<i32> {
    let rest = line.strip_prefix("__STOAT_")?;
    let after_id = rest.find("__ ")?;
    let status_str = &rest[after_id + 3..];
    status_str.trim().parse().ok()
}
