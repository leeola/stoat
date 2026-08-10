use async_trait::async_trait;
use portable_pty::CommandBuilder;
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc as sync_mpsc, Arc, Mutex,
    },
};
use tokio::sync::mpsc;

/// Queued-write count that trips the writer-stall warning.
///
/// When this many chunks sit unwritten the child has stopped draining its
/// input, which is the mechanism behind a terminal that looks frozen.
const WRITE_STALL_THRESHOLD: usize = 256;

/// Process-spawn parameters for [`TerminalHost::spawn`]. Carries the data
/// needed to build a [`portable_pty::CommandBuilder`] and size the PTY.
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Environment variables to unset on the child, removing them from the
    /// inherited environment. Applied before [`Self::env`] so a set there
    /// wins over an unset here on the same key.
    pub env_remove: Vec<String>,
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
/// process introspection), a queue into the shell's input, the child
/// handle, and a reader thread that pumps output into a channel.
pub(crate) struct PtyTerminalSession {
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: sync_mpsc::Sender<Vec<u8>>,
    /// Chunks sent but not yet written, shared with the writer thread.
    write_queue_depth: Arc<AtomicUsize>,
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

        let write_queue_depth = Arc::new(AtomicUsize::new(0));

        Self {
            master: Mutex::new(master),
            writer: spawn_writer(writer, write_queue_depth.clone()),
            write_queue_depth,
            child: Mutex::new(child),
            read_rx: tokio::sync::Mutex::new(rx),
        }
    }
}

#[async_trait]
impl TerminalSession for PtyTerminalSession {
    /// Queue `data` for the shell's input, to be written and flushed by the
    /// writer thread.
    ///
    /// Returns once the bytes are queued, never blocking on the write itself,
    /// so a child that has stopped draining its input parks the writer thread
    /// rather than the caller. The queue is unbounded, so nothing is dropped
    /// while that lasts.
    ///
    /// An [`io::ErrorKind::BrokenPipe`] error means the writer thread has
    /// exited, which happens once a write to the PTY fails.
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        let queued = self.write_queue_depth.fetch_add(1, Ordering::Relaxed);
        if queued == WRITE_STALL_THRESHOLD {
            tracing::warn!(
                target: "stoat::agent",
                queued = queued + 1,
                "pty writer backlog crossed {WRITE_STALL_THRESHOLD}; the child is not draining its input"
            );
        }

        self.writer
            .send(data.to_vec())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
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
    for key in &args.env_remove {
        cmd.env_remove(key);
    }
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    cmd.cwd(&args.cwd);

    let child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
    let writer = pair.master.take_writer().map_err(io::Error::other)?;
    let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;

    Ok(PtyTerminalSession::new(pair.master, writer, child, reader))
}

/// Fold a project-environment diff into `args`, so a child spawn applies
/// the workspace's direnv overrides.
///
/// Each `Some(value)` entry is appended to [`SpawnArgs::env`] and each
/// `None` key to [`SpawnArgs::env_remove`]. Producers call this before
/// appending their own built-in vars, so those built-ins land later in
/// `env` and win any key conflict under the last-writer semantics of
/// [`open_local_pty`].
pub(crate) fn merge_env_diff(args: &mut SpawnArgs, diff: &[(String, Option<String>)]) {
    for (key, value) in diff {
        match value {
            Some(value) => args.env.push((key.clone(), value.clone())),
            None => args.env_remove.push(key.clone()),
        }
    }
}

/// Resolve a pid to its command name from the OS process table. Linux
/// reads `/proc/<pid>/comm`. macOS calls `libc::proc_name`. Other targets
/// return `None`.
// Reads the OS process table via /proc, which is process introspection rather
// than the user-file IO the FsHost abstracts.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)]
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

/// Spawn a thread that writes each received chunk to `writer`, flushing after
/// each so the shell sees input promptly. Returns the sender that feeds it.
///
/// `depth` is decremented after each chunk is written, mirroring the increment
/// on send, so it tracks the queued-but-unwritten backlog rather than a total.
///
/// This is what decouples a write from the blocking syscall behind it. A child
/// that stops draining its input parks the `write_all` on this thread instead
/// of on the run loop, where it would freeze every pane and all input.
///
/// The thread ends when the channel closes or a write fails, so dropping the
/// session ends it. The sender drops to unblock an idle writer, and killing the
/// child closes the PTY to unblock a parked one. Nothing joins it: something has
/// to park while a child refuses to read, and a detached thread is the one place
/// that costs nothing.
fn spawn_writer(
    mut writer: Box<dyn io::Write + Send>,
    depth: Arc<AtomicUsize>,
) -> sync_mpsc::Sender<Vec<u8>> {
    let (tx, rx) = sync_mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(bytes) = rx.recv() {
            let wrote = writer.write_all(&bytes).and_then(|()| writer.flush());
            depth.fetch_sub(1, Ordering::Relaxed);
            if wrote.is_err() {
                break;
            }
        }
    });
    tx
}

fn blocking_read_loop(mut reader: Box<dyn io::Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    // 64KB matches stoatty's own PTY reader, so a high-throughput stream sends
    // whole chunks rather than fragmenting into 16x the messages and allocations.
    let mut buf = vec![0u8; 64 * 1024];
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
    use std::sync::Condvar;

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
                env_remove: Vec::new(),
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

    /// An in-memory [`io::Write`] that parks every write on a gate until it
    /// opens, then records the bytes, standing in for a PTY master whose child
    /// has stopped draining its input.
    ///
    /// It also samples the backlog gauge on the way past. Reading it from the
    /// writer's own thread orders the sample against the writer's decrements,
    /// which reading it from the test thread would not.
    struct GatedWriter {
        gate: Arc<(Mutex<bool>, Condvar)>,
        written: Arc<(Mutex<Vec<u8>>, Condvar)>,
        depth: Arc<AtomicUsize>,
        sampled: Arc<Mutex<Vec<usize>>>,
    }

    impl io::Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (open_lock, open_cvar) = &*self.gate;
            let mut open = open_lock.lock().unwrap();
            while !*open {
                open = open_cvar.wait(open).unwrap();
            }
            drop(open);

            self.sampled
                .lock()
                .unwrap()
                .push(self.depth.load(Ordering::Relaxed));

            let (buf_lock, buf_cvar) = &*self.written;
            buf_lock.lock().unwrap().extend_from_slice(buf);
            buf_cvar.notify_all();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A child that has stopped reading parks the writer thread, not the
    /// caller.
    ///
    /// This is the whole point of the thread. Every send comes from the run
    /// loop, so a write that blocked there would freeze every pane, all input,
    /// and rendering until the child read again. The backlog is counted rather
    /// than bounded, since dropping a keystroke is the other way to lose.
    ///
    /// Counted is also the claim worth checking. A gauge that only ever rose
    /// would read as a total, and the stall warning would fire on any long
    /// session rather than on a child that stopped reading.
    #[test]
    fn a_parked_writer_does_not_block_the_sender() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let written = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let depth = Arc::new(AtomicUsize::new(0));
        let sampled = Arc::new(Mutex::new(Vec::new()));
        let tx = spawn_writer(
            Box::new(GatedWriter {
                gate: gate.clone(),
                written: written.clone(),
                depth: depth.clone(),
                sampled: sampled.clone(),
            }),
            depth.clone(),
        );

        for chunk in [b"foo".as_slice(), b"bar".as_slice()] {
            depth.fetch_add(1, Ordering::Relaxed);
            tx.send(chunk.to_vec()).unwrap();
        }
        assert!(
            written.0.lock().unwrap().is_empty(),
            "the sender returns while the writer is parked, so nothing is written yet",
        );
        assert_eq!(
            depth.load(Ordering::Relaxed),
            2,
            "both queued chunks count toward the backlog while the writer is parked",
        );

        {
            let (open_lock, open_cvar) = &*gate;
            *open_lock.lock().unwrap() = true;
            open_cvar.notify_all();
        }

        let (buf_lock, buf_cvar) = &*written;
        let mut got = buf_lock.lock().unwrap();
        while got.len() < 6 {
            got = buf_cvar.wait(got).unwrap();
        }
        assert_eq!(
            got.as_slice(),
            b"foobar",
            "the queued chunks are written in order once the gate opens",
        );
        drop(got);

        assert_eq!(
            sampled.lock().unwrap().as_slice(),
            [2, 1],
            "the backlog falls as each chunk is written, rather than only rising",
        );
    }

    #[test]
    fn merge_env_diff_appends_sets_and_records_unsets() {
        let mut args = SpawnArgs {
            program: "x".into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: PathBuf::from("/"),
            width: 80,
            rows: 24,
        };
        let diff = vec![
            ("SET_ME".to_string(), Some("val".to_string())),
            ("UNSET_ME".to_string(), None),
        ];
        merge_env_diff(&mut args, &diff);
        // A built-in appended after the diff lands later in env, so it wins
        // the key conflict under open_local_pty's last-writer semantics.
        args.env.push(("SET_ME".into(), "builtin".into()));

        assert_eq!(
            args.env,
            vec![
                ("SET_ME".to_string(), "val".to_string()),
                ("SET_ME".to_string(), "builtin".to_string()),
            ]
        );
        assert_eq!(args.env_remove, vec!["UNSET_ME".to_string()]);
    }
}
