use async_trait::async_trait;
use portable_pty::CommandBuilder;
use std::{io, path::PathBuf, sync::Mutex};
use tokio::sync::mpsc;

/// Process-spawn parameters for [`TerminalHost::spawn`]. Carries
/// the data both [`crate::run::spawn_shell`] (the persistent
/// shell variant) and [`crate::run::spawn_oneshot`] (the single
/// command variant) need to build a [`portable_pty::CommandBuilder`].
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub width: u16,
}

/// Per-PTY I/O handle returned by [`TerminalHost::spawn`].
/// Implementors expose write/read against the PTY plus a
/// best-effort kill of the spawned child.
#[async_trait]
pub trait TerminalSession: Send + Sync {
    async fn write(&self, data: &[u8]) -> io::Result<()>;
    async fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    async fn kill(&self) -> io::Result<()>;

    /// The exit code if the command has finished, or `None` if it is
    /// still running. Non-blocking; callers detect completion via a
    /// `read` returning `Ok(0)` and then read the code here.
    async fn try_wait(&self) -> io::Result<Option<i32>>;
}

/// Factory that opens new PTY-backed terminal sessions.
/// Production wires [`crate::host::local::LocalTerminalHost`];
/// tests wire a fake that returns a pre-configured
/// [`crate::host::fake::terminal::FakeTerminalSession`].
#[async_trait]
pub trait TerminalHost: Send + Sync {
    async fn spawn(&self, args: SpawnArgs) -> io::Result<Box<dyn TerminalSession>>;
}

pub struct PtyTerminalSession {
    writer: Mutex<Box<dyn io::Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    leftover: tokio::sync::Mutex<Vec<u8>>,
}

impl PtyTerminalSession {
    pub(crate) fn new(
        writer: Box<dyn io::Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        reader: Box<dyn io::Read + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);

        std::thread::spawn(move || {
            blocking_read_loop(reader, tx);
        });

        Self {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            read_rx: tokio::sync::Mutex::new(rx),
            leftover: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl TerminalSession for PtyTerminalSession {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        writer.write_all(data)?;
        writer.flush()
    }

    async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut leftover = self.leftover.lock().await;
        if !leftover.is_empty() {
            let n = leftover.len().min(buf.len());
            buf[..n].copy_from_slice(&leftover[..n]);
            leftover.drain(..n);
            return Ok(n);
        }
        drop(leftover);

        let mut rx = self.read_rx.lock().await;
        match rx.recv().await {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    let mut leftover = self.leftover.lock().await;
                    leftover.extend_from_slice(&chunk[n..]);
                }
                Ok(n)
            },
            None => Ok(0),
        }
    }

    async fn kill(&self) -> io::Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        child.kill().map_err(io::Error::other)
    }

    async fn try_wait(&self) -> io::Result<Option<i32>> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(child.try_wait()?.map(|status| status.exit_code() as i32))
    }
}

/// Synchronous PTY-open helper shared between
/// [`LocalTerminalHost::spawn`] and the legacy
/// [`crate::run::spawn_shell`] / [`crate::run::spawn_oneshot`]
/// entry points. Lives here so both call sites use the same
/// portable-pty boilerplate without an async detour.
pub(crate) fn open_local_pty(args: SpawnArgs) -> io::Result<PtyTerminalSession> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: args.width,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(io::Error::other)?;

    let mut cmd = CommandBuilder::new(args.program);
    cmd.args(&args.args);
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    cmd.cwd(&args.cwd);

    let child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
    let writer = pair.master.take_writer().map_err(io::Error::other)?;
    let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;

    Ok(PtyTerminalSession::new(writer, child, reader))
}

fn blocking_read_loop(mut reader: Box<dyn io::Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if tx.blocking_send(buf[..n].to_vec()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::local::LocalTerminalHost;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn local_host_spawns_and_reads_output() {
        rt().block_on(async {
            let host = LocalTerminalHost;
            let args = SpawnArgs {
                program: "bash".into(),
                args: vec!["-c".into(), "printf hello".into()],
                env: vec![("TERM".into(), "dumb".into())],
                cwd: std::env::temp_dir(),
                width: 80,
            };
            let session = host.spawn(args).await.expect("spawn");
            let mut collected = Vec::new();
            let mut buf = [0u8; 64];
            while let Ok(n) = session.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&buf[..n]);
                if collected.windows(5).any(|w| w == b"hello") {
                    break;
                }
            }
            assert!(
                collected.windows(5).any(|w| w == b"hello"),
                "expected hello in output, got {collected:?}",
            );
        });
    }
}
