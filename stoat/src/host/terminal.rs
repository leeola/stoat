use async_trait::async_trait;
use portable_pty::CommandBuilder;
use std::{io, path::PathBuf, sync::Mutex};
use tokio::sync::mpsc;

/// Process-spawn parameters for [`TerminalHost::spawn`]. Carries
/// the data [`crate::run::spawn_shell`] needs to build a
/// [`portable_pty::CommandBuilder`].
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub width: u16,
    pub rows: u16,
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

    /// Resize the PTY to `rows` x `cols` character cells, which signals
    /// the foreground process (SIGWINCH on Unix). Synchronous so the
    /// renderer can call it inline as the cell grid changes.
    fn resize(&self, rows: u16, cols: u16) -> io::Result<()>;

    /// The command name of the PTY's current foreground process (the one
    /// reading input), or `None` when it cannot be determined. Used to
    /// reflect the running program in the terminal's tab. Defaults to
    /// `None` for sessions without process introspection.
    fn foreground_process_name(&self) -> Option<String> {
        None
    }
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
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Mutex<Box<dyn io::Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    leftover: tokio::sync::Mutex<Vec<u8>>,
}

impl PtyTerminalSession {
    pub(crate) fn new(
        master: Box<dyn portable_pty::MasterPty + Send>,
        writer: Box<dyn io::Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        reader: Box<dyn io::Read + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);

        std::thread::spawn(move || {
            blocking_read_loop(reader, tx);
        });

        Self {
            master: Mutex::new(master),
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

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let master = self
            .master
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    fn foreground_process_name(&self) -> Option<String> {
        let pid = self.master.lock().ok()?.process_group_leader()?;
        process_name_for_pid(pid)
    }
}

/// Resolve a pid to its command name. Platform-specific: Linux reads
/// `/proc/<pid>/comm`; macOS calls `libc::proc_name`; other targets return
/// `None`.
#[cfg(target_os = "linux")]
fn process_name_for_pid(pid: libc::pid_t) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = comm.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(target_os = "macos")]
fn process_name_for_pid(pid: libc::pid_t) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `proc_name` writes at most `buf.len()` bytes into `buf` and
    // returns the count written; the buffer outlives the call.
    let written =
        unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if written <= 0 {
        return None;
    }
    let name = std::str::from_utf8(&buf[..written as usize]).ok()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_name_for_pid(_pid: libc::pid_t) -> Option<String> {
    None
}

/// Synchronous PTY-open helper shared between
/// [`LocalTerminalHost::spawn`] and the legacy
/// [`crate::run::spawn_shell`] entry point. Lives here so both
/// call sites use the same portable-pty boilerplate without an
/// async detour.
pub(crate) fn open_local_pty(args: SpawnArgs) -> io::Result<PtyTerminalSession> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: args.rows,
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

    Ok(PtyTerminalSession::new(pair.master, writer, child, reader))
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
                rows: 24,
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

    #[test]
    fn local_host_reads_foreground_process_name() {
        rt().block_on(async {
            let host = LocalTerminalHost;
            // A non-multicall binary so the resolved name is the program
            // itself (coreutils-style busyboxes report the wrapper name).
            let args = SpawnArgs {
                program: "bash".into(),
                args: vec!["--norc".into(), "--noprofile".into()],
                env: vec![("TERM".into(), "dumb".into())],
                cwd: std::env::temp_dir(),
                width: 80,
                rows: 24,
            };
            let session = host.spawn(args).await.expect("spawn");

            // The child forks then execs bash; retry until the foreground
            // process group leader resolves to the exec'd program.
            let mut last = None;
            for _ in 0..50 {
                last = session.foreground_process_name();
                if last.as_deref() == Some("bash") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let _ = session.kill().await;

            assert_eq!(
                last.as_deref(),
                Some("bash"),
                "foreground process of a bash PTY should be bash, last saw {last:?}",
            );
        });
    }
}
