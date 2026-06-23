use async_trait::async_trait;
use portable_pty::CommandBuilder;
use std::{io, path::PathBuf, sync::Mutex};
use tokio::sync::mpsc;

/// Process-spawn parameters for [`TerminalHost::spawn`]. Carries the data
/// needed to build a [`portable_pty::CommandBuilder`] and size the PTY.
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub width: u16,
    pub rows: u16,
}

/// Per-PTY I/O handle returned by [`TerminalHost::spawn`]. Implementors
/// expose write/read against the PTY, a best-effort kill of the spawned
/// child, and metadata (exit code, size, foreground process).
#[async_trait]
pub trait TerminalSession: Send + Sync {
    async fn write(&self, data: &[u8]) -> io::Result<()>;

    /// The next chunk of shell output, or `None` once the shell closes its
    /// end. Each call returns one read's worth of bytes and hands ownership
    /// to the caller, so the chunk can move on without a further copy.
    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>>;

    async fn kill(&self) -> io::Result<()>;

    /// The exit code if the command has finished, or `None` if it is still
    /// running. Non-blocking. Callers detect completion via a `read_chunk`
    /// returning `None`, then read the code here.
    async fn try_wait(&self) -> io::Result<Option<i32>>;

    /// Resize the PTY to `rows` x `cols` character cells, which signals the
    /// foreground process (SIGWINCH on Unix). Synchronous so the renderer
    /// can call it inline as the cell grid changes.
    fn resize(&self, rows: u16, cols: u16) -> io::Result<()>;

    /// The command name of the PTY's current foreground process (the one
    /// reading input), or `None` when it cannot be determined. Used to
    /// reflect the running program in the terminal's tab. Defaults to
    /// `None` for sessions without process introspection.
    fn foreground_process_name(&self) -> Option<String> {
        None
    }
}

/// Factory that opens new PTY-backed terminal sessions. Production wires
/// [`crate::host::local::LocalTerminalHost`]. Tests wire a fake that
/// returns a pre-configured [`crate::host::fake::terminal::FakeTerminalSession`].
#[async_trait]
pub trait TerminalHost: Send + Sync {
    async fn spawn(&self, args: SpawnArgs) -> io::Result<Box<dyn TerminalSession>>;
}

/// PTY-backed [`TerminalSession`]: owns the master (for resizing and
/// process introspection), the writer to the shell's input, the child
/// handle, and a reader thread that pumps output into a channel.
pub(crate) struct PtyTerminalSession {
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Mutex<Box<dyn io::Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
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

    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.read_rx.lock().await.recv().await)
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

/// Synchronous PTY-open helper shared between [`TerminalHost::spawn`]
/// (via [`crate::host::local::LocalTerminalHost`]) and the legacy
/// [`crate::run::spawn_shell`] entry point, so both build the PTY through
/// the same portable-pty boilerplate without an async detour.
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

/// Resolve a pid to its command name from the OS process table. Linux
/// reads `/proc/<pid>/comm`. macOS calls `libc::proc_name`. Other targets
/// return `None`.
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
    // returns the count written. The buffer outlives the call.
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
            while let Ok(Some(chunk)) = session.read_chunk().await {
                collected.extend_from_slice(&chunk);
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
