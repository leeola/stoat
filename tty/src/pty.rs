//! The PTY shell: spawns a child shell over a pseudoterminal and pumps its
//! output to the app on a dedicated reader thread.
//!
//! Lives in the app crate, off the terminal-core crates, so [`stoatty_term`]
//! stays a pure bytes-to-grid model. The reader thread feeds bytes back through
//! a caller-supplied sink, which the app forwards onto the winit event loop, so
//! a blocking PTY read never stalls rendering. A dedicated writer thread absorbs
//! blocking writes the same way, so a child that stops draining its input parks
//! that thread rather than the UI thread that forwards key presses.

use libc::passwd;
use portable_pty::{self, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use std::{
    ffi::{c_char, CStr, OsString},
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    ptr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Sender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// A chunk of PTY activity handed to the [`Pty::spawn`] sink.
///
/// [`PtyOutput::Data`] borrows the reader thread's read buffer, so it is valid
/// only for the duration of the sink call. The sink must not retain the slice,
/// since the next read overwrites the buffer.
pub(crate) enum PtyOutput<'a> {
    /// Bytes read from the shell, borrowed from the reader buffer, to feed into
    /// the parser.
    ///
    /// The sink reports whether it took them. Refusing is how a sink that would
    /// have to wait for something keeps the reader out of that wait, and the
    /// bytes come back on a later call. `may_refuse` is clear when the reader
    /// has nowhere left to hold them, and the call is then treated as taken
    /// whatever it answers.
    Data { bytes: &'a [u8], may_refuse: bool },
    /// The shell closed its end; no more data will follow.
    Eof,
}

/// A running shell attached to a pseudoterminal.
///
/// Owns the PTY master (for resizing), a channel to the writer thread that
/// forwards key presses to the shell's input, the child handle, and the reader
/// thread. Dropping it kills the shell, which closes the slave and ends both
/// worker threads, so closing the window tears the shell down.
pub(crate) struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Sender<Vec<u8>>,
    write_queue_depth: Arc<AtomicUsize>,
    child: Box<dyn Child + Send + Sync>,
    _reader: JoinHandle<()>,
}

/// Queued-write count that trips the writer-stall warning. When this many chunks
/// sit unwritten the child has stopped draining its input, which is the
/// mechanism behind a frozen terminal.
const WRITE_STALL_THRESHOLD: usize = 256;

impl Pty {
    /// Spawn `program` with `args` over a fresh PTY sized `rows` by `cols` and
    /// start reading. Runs the command in `cwd` when given. A `None` cwd lets
    /// portable_pty default the working directory to the home directory, so
    /// callers that want a specific cwd resolve it before calling here.
    ///
    /// When `stoat_dir` is set, that directory is prepended to the child's
    /// `PATH`, so a nested bare-`stoat` call from inside the child resolves to
    /// the same binary stoatty launched.
    ///
    /// `sink` runs on the reader thread: it is called with [`PtyOutput::Data`]
    /// for each chunk the shell writes and once with [`PtyOutput::Eof`] when the
    /// shell exits. It must be `Send` since it runs off the main thread.
    ///
    /// A sink that would have to wait to take a chunk should say so rather than
    /// wait, since the reader cannot drain the pty while it is inside the sink
    /// and a full pty stops the child. Refused bytes are offered again.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        stoat_dir: Option<&Path>,
        log_id: Option<&str>,
        window_socket: Option<&str>,
        theme: &str,
        rows: u16,
        cols: u16,
        sink: impl FnMut(PtyOutput<'_>) -> bool + Send + 'static,
    ) -> io::Result<Pty> {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;

        let child = pair
            .slave
            .spawn_command(shell_command(
                program,
                args,
                cwd,
                stoat_dir,
                log_id,
                window_socket,
                theme,
            ))
            .map_err(io::Error::other)?;
        tracing::info!(program, pid = ?child.process_id(), "spawned child over pty");
        let master_writer = pair.master.take_writer().map_err(io::Error::other)?;
        let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let poll_fd = dup_for_polling(pair.master.as_ref());

        let reader_thread = thread::spawn(move || {
            read_loop(
                reader,
                || poll_fd.as_ref().is_some_and(fd_readable_now),
                sink,
            )
        });

        let write_queue_depth = Arc::new(AtomicUsize::new(0));
        Ok(Pty {
            master: pair.master,
            writer: spawn_writer(master_writer, write_queue_depth.clone()),
            write_queue_depth,
            child,
            _reader: reader_thread,
        })
    }

    /// Resize the PTY to `rows` by `cols` so the shell learns the new geometry.
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    /// Queue `bytes` for the shell's input, to be written and flushed by the
    /// writer thread.
    ///
    /// Encoded key presses flow through here, so typing in the window reaches
    /// the shell. The call returns as soon as the bytes are queued and never
    /// blocks on the write itself, so a child that has stopped draining its
    /// input cannot stall the UI thread. An [`io::ErrorKind::BrokenPipe`] error
    /// means the writer thread has exited.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let queued = self.write_queue_depth.fetch_add(1, Ordering::Relaxed);
        if queued == WRITE_STALL_THRESHOLD {
            tracing::warn!(
                queued = queued + 1,
                "pty writer backlog crossed {WRITE_STALL_THRESHOLD}; the child is not draining its input"
            );
        }
        self.writer
            .send(bytes.to_vec())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }

    /// Wait up to `timeout` for the child's exit status, or `None` if it has
    /// not exited by then.
    ///
    /// A child can close the pty yet linger before exiting, so this polls
    /// rather than reading the status once. The bound matters because this runs
    /// on the main thread at shutdown, where a child that never exits must not
    /// stall the window close.
    pub(crate) fn exit_status(&mut self, timeout: Duration) -> Option<ExitStatus> {
        wait_exit_status(self.child.as_mut(), timeout, Duration::from_millis(10))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Spawn a thread that writes each received chunk to `writer`, flushing after
/// each so the shell sees input promptly. Returns the sender that feeds it.
///
/// Decrements `depth` after writing each chunk, mirroring [`Pty::write`]'s
/// increment on send, so `depth` tracks the queued-but-unwritten backlog.
///
/// Decouples [`Pty::write`] from the blocking write on the PTY master. A child
/// that stops draining its input parks the `write_all` on this thread rather
/// than on the caller. The thread exits when the channel closes or a write
/// fails, so dropping the [`Pty`] ends it: the sender drops to unblock an idle
/// writer, and killing the child closes the slave to unblock a parked one.
fn spawn_writer(mut writer: Box<dyn Write + Send>, depth: Arc<AtomicUsize>) -> Sender<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
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

/// Build the shell command to launch under stoatty, running in `cwd` when
/// given. [`configure_child_env`] sets the environment the child inherits.
fn shell_command(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    stoat_dir: Option<&Path>,
    log_id: Option<&str>,
    window_socket: Option<&str>,
    theme: &str,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.cwd(dir);
    }
    configure_child_env(&mut command, stoat_dir, log_id, window_socket, theme);
    command
}

/// Multiplexer markers stripped from the child environment so a program run
/// under stoatty detects stoatty rather than an outer `tmux`/`zellij`. `TMUX`
/// and `ZELLIJ` are the markers consumers check. The companions are stripped
/// too so the child environment is coherent rather than half-cleared.
const MULTIPLEXER_ENV_VARS: [&str; 5] = [
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
];

/// Set the environment a stoatty child shell inherits.
///
/// `TERM` selects the terminfo the shell and its children load.
///
/// `TERM_PROGRAM` and `TERM_PROGRAM_VERSION` name the terminal itself, the pair
/// the rest of the ecosystem already reads to tell one terminal from another.
/// Assigning them also replaces whatever an outer terminal exported, so a
/// stoatty launched from another program does not leave its children reading
/// that program's name. `TERM_PROGRAM_VERSION` is a bare semver, since a
/// consumer compares it.
///
/// `STOATTY` marks the shell as running under stoatty, so a child can gate
/// stoatty-only output on its presence synchronously at startup, without the
/// `XTVERSION` query round trip. It stays the marker stoat's own detection
/// reads, since `TERM_PROGRAM` says which terminal this is and not that it
/// speaks the protocol. `STOATTY_VERSION` carries the full build string rather
/// than the bare version, for a bug report that has to name the exact build.
///
/// `STOATTY_LOG_ID`, when set, carries this session's log id so a child
/// prefixes its own log filenames with it and one session's files sort together.
/// `STOATTY_WINDOW_SOCKET`, when set, is the unix-socket path a child editor
/// connects to for upstream window focus, resize, and close events.
/// `STOAT_THEME` carries stoatty's configured theme name so a child editor
/// adopts the terminal's theme by default. An empty `theme` exports nothing,
/// leaving the child on its own configured default.
///
/// Inherited multiplexer markers ([`MULTIPLEXER_ENV_VARS`]) are removed because
/// stoatty presents a fresh terminal that owns and forwards its own window's
/// mouse, so a program inside it must detect stoatty and enable mouse
/// reporting rather than stand down for an outer `tmux`/`zellij` that does not
/// control this window. A real multiplexer launched inside stoatty re-sets
/// these for its own children, so in-stoatty-mux detection still stands down as
/// intended.
// PATH is inherited to be extended, not consulted, and EnvHost::var yields only
// UTF-8, which a PATH is not required to be. prepend_path holds the pure part.
#[allow(clippy::disallowed_methods)]
fn configure_child_env(
    command: &mut CommandBuilder,
    stoat_dir: Option<&Path>,
    log_id: Option<&str>,
    window_socket: Option<&str>,
    theme: &str,
) {
    command.env("TERM", "xterm-256color");
    command.env("TERM_PROGRAM", "stoatty");
    command.env("TERM_PROGRAM_VERSION", crate::cli::PACKAGE_VERSION);
    command.env("STOATTY", "1");
    command.env("STOATTY_VERSION", crate::cli::VERSION_INFO);
    if let Some(id) = log_id {
        command.env("STOATTY_LOG_ID", id);
    }
    if let Some(socket) = window_socket {
        command.env("STOATTY_WINDOW_SOCKET", socket);
    }
    if !theme.is_empty() {
        command.env("STOAT_THEME", theme);
    }
    for var in MULTIPLEXER_ENV_VARS {
        command.env_remove(var);
    }
    if let Some(dir) = stoat_dir {
        command.env("PATH", prepend_path(dir, std::env::var_os("PATH")));
    }
}

/// Prepend `dir` to a `PATH`-style variable so a binary in `dir` is found
/// before the existing entries. Yields just `dir` when `existing` is absent or
/// empty, so no stray separator is appended.
fn prepend_path(dir: &Path, existing: Option<OsString>) -> OsString {
    let mut path = dir.as_os_str().to_os_string();
    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        path.push(if cfg!(windows) { ";" } else { ":" });
        path.push(existing);
    }
    path
}

/// How much output one burst may accumulate before the reader hands it on.
///
/// A pty's line discipline returns a few KiB per read, while the sink takes the
/// terminal lock and runs a whole parse pass, so the buffer's job is to bound
/// how much of a flood is folded into one of those. Large enough that a `cat` of
/// a big file costs a handful of parses rather than hundreds, small enough that
/// the lock is never held for an unbounded stretch. Heap-allocated because a
/// quarter-megabyte would not fit comfortably on the reader thread's stack.
const READ_BUF_SIZE: usize = 256 * 1024;

/// Most that goes to the sink in one call.
///
/// The sink parses under the terminal lock, so a call is a stretch the main
/// thread cannot render in. A buffer's worth handed over whole would put a
/// quarter-megabyte parse in front of the next frame, where four smaller calls
/// leave four points where the frame can go first. Splitting costs nothing:
/// both the VT parser and the escape scanner carry their state between calls,
/// so a sequence cut in half is resumed rather than lost.
const HANDOFF_SLICE: usize = 64 * 1024;

/// Pump `reader` to `sink` until end of input, accumulating what arrives
/// together into one [`PtyOutput::Data`] chunk, then one [`PtyOutput::Eof`] once
/// the shell closes its end or the read errors.
///
/// `readable_now` reports whether more output is already waiting, and is the
/// only thing holding a burst open. One that always answers false hands every
/// read on by itself, which is what to pass when nothing can tell.
fn read_loop(
    mut reader: impl Read,
    readable_now: impl FnMut() -> bool,
    sink: impl FnMut(PtyOutput<'_>) -> bool,
) {
    accumulate_loop(move |buf| reader.read(buf), readable_now, sink);
}

/// [`read_loop`]'s pump with the IO lifted into closures, so the accumulation
/// rule can be exercised without a pty.
///
/// Reads into the unfilled tail of one reused buffer, and keeps reading while
/// `readable_now` says more is already pending. Pending means the next read
/// returns without blocking, so folding it into this burst costs no latency.
/// Flushes when nothing more is waiting or the buffer fills, and flushes
/// whatever it holds before reporting the end of input.
///
/// A sink that refuses a chunk is not waited on while there is still output to
/// take in, so a busy consumer leaves the reader draining the kernel's buffer
/// rather than parked. The child keeps running either way, which is the point:
/// a reader that stops reading fills the pty and blocks the child in `write`.
fn accumulate_loop(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    mut readable_now: impl FnMut() -> bool,
    mut sink: impl FnMut(PtyOutput<'_>) -> bool,
) {
    let mut buf = vec![0u8; READ_BUF_SIZE].into_boxed_slice();
    let mut filled = 0;

    loop {
        // A forced hand-off empties the buffer, so this slice is never empty
        // and a read cannot return zero for want of room, which would read as
        // end of input.
        match read(&mut buf[filled..]) {
            Ok(0) | Err(_) => {
                if filled > 0 {
                    hand_off(&mut buf, filled, false, &mut sink);
                }
                break;
            },
            Ok(n) => {
                filled += n;
                if filled < buf.len() && readable_now() {
                    continue;
                }

                filled = hand_off(&mut buf, filled, true, &mut sink);

                // Refused. Deferring is only safe while output is waiting,
                // since the read below would otherwise park on a quiet child
                // and leave what is held here with nothing to release it. A
                // full buffer cannot defer at all, having nowhere to read into.
                if filled > 0 && !(filled < buf.len() && readable_now()) {
                    filled = hand_off(&mut buf, filled, false, &mut sink);
                }
            },
        }
    }

    sink(PtyOutput::Eof);
}

/// Offer `buf[..filled]` to `sink` in slices of at most [`HANDOFF_SLICE`], and
/// report what is left over, moved to the front of `buf`.
///
/// Returns zero when `may_refuse` is clear, since a call the sink may not
/// refuse is taken whatever it answers.
fn hand_off(
    buf: &mut [u8],
    filled: usize,
    may_refuse: bool,
    sink: &mut impl FnMut(PtyOutput<'_>) -> bool,
) -> usize {
    let mut start = 0;
    while start < filled {
        let end = (start + HANDOFF_SLICE).min(filled);
        let taken = sink(PtyOutput::Data {
            bytes: &buf[start..end],
            may_refuse,
        });
        if may_refuse && !taken {
            break;
        }
        start = end;
    }

    if start == 0 {
        return filled;
    }
    buf.copy_within(start..filled, 0);
    filled - start
}

/// Whether `fd` has output waiting, answered without blocking.
///
/// False on any answer other than a definite yes. A poll error or a descriptor
/// the kernel rejects leaves the caller flushing per read, which is correct and
/// merely misses the batching.
fn fd_readable_now(fd: &OwnedFd) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, borrowed for the length of the call, with
    // a zero timeout so it cannot block.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    ready > 0 && poll_fd.revents & libc::POLLNVAL == 0 && poll_fd.revents & libc::POLLIN != 0
}

/// A private duplicate of the pty master's descriptor for the reader thread to
/// poll, or `None` if the master has no descriptor or the dup fails.
///
/// Duplicated rather than borrowed so polling it cannot race the master being
/// closed or resized from the main thread.
fn dup_for_polling(master: &(dyn MasterPty + Send)) -> Option<OwnedFd> {
    dup_fd(master.as_raw_fd()?)
}

/// Duplicate `raw`, or `None` if the kernel refuses.
///
/// The duplicate is a descriptor nobody else holds, so the returned [`OwnedFd`]
/// is solely responsible for closing it and the original is untouched.
fn dup_fd(raw: i32) -> Option<OwnedFd> {
    // SAFETY: dup is called on a descriptor its owner still holds open, and it
    // returns a fresh one that no other value claims.
    let duped = unsafe { libc::dup(raw) };
    match duped < 0 {
        true => None,
        // SAFETY: from_raw_fd takes the sole ownership dup just handed over.
        false => Some(unsafe { OwnedFd::from_raw_fd(duped) }),
    }
}

/// Leave `tail` holding the newest `cap` bytes of itself followed by `bytes`.
///
/// Maintains a bounded rolling window of the child's most recent output, held
/// for the diagnostic logged when the pty closes.
pub(crate) fn push_tail(tail: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    // A chunk at least as long as the cap supersedes the tail outright, so its own
    // trailing bytes are the whole answer and the existing contents need no copy.
    // Appending first would copy the chunk whole and discard nearly all of it, and a
    // read chunk runs many times the cap.
    if bytes.len() >= cap {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - cap..]);
        return;
    }

    tail.extend_from_slice(bytes);
    if tail.len() > cap {
        tail.drain(..tail.len() - cap);
    }
}

/// Strip terminal escape sequences and control characters from `text`, leaving
/// human-readable output fit for a single log line.
///
/// Removes CSI sequences (`ESC [` through a final byte in `0x40..=0x7e`), OSC
/// sequences (`ESC ]` through `BEL` or the `ESC \` string terminator), other
/// two-character `ESC` sequences, and control characters other than newline,
/// then trims surrounding whitespace. The child's output tail is captured raw,
/// so it carries the cursor moves and color codes that would otherwise render
/// the log line unreadable.
pub(crate) fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            if ch == '\n' || !ch.is_control() {
                out.push(ch);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            },
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            },
            _ => {},
        }
    }
    out.trim().to_string()
}

/// Poll `child` every `poll` interval until it reports an exit status or
/// `timeout` elapses, returning `None` on timeout or a wait error.
///
/// Split out from [`Pty::exit_status`] so the polling loop can be driven by a
/// stub child in tests. Always calls `try_wait` at least once, so a child that
/// has already exited is reported without waiting a full `poll` interval.
fn wait_exit_status(
    child: &mut (dyn Child + Send + Sync),
    timeout: Duration,
    poll: Duration,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {},
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(poll);
    }
}

/// The shell to launch, in order of preference: `$SHELL`, the passwd entry's
/// login shell, then `/bin/sh`.
///
/// Used for the `--terminal` opt-out, which runs the login shell instead of the
/// stoat editor.
// The env read is the blessed boundary. It hands the value to shell_or_default,
// which applies the precedence and is unit-tested.
#[allow(clippy::disallowed_methods)]
pub(crate) fn default_shell() -> String {
    shell_or_default(std::env::var("SHELL").ok(), passwd_shell())
}

/// The first non-empty shell among the candidates, most preferred first: the
/// `$SHELL` value, then the passwd login shell, then `/bin/sh` as the last
/// resort.
///
/// A candidate that is `None` or empty is skipped, so an unset or blank source
/// falls through to the next.
fn shell_or_default(env_shell: Option<String>, passwd_shell: Option<String>) -> String {
    env_shell
        .into_iter()
        .chain(passwd_shell)
        .find(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// The current user's login shell from the passwd database, or `None` when the
/// entry cannot be read or records no shell.
///
/// Reads the entry for the real user id via `getpwuid_r`. An empty `pw_shell`
/// (some entries leave it blank) yields `None` so the caller falls back.
fn passwd_shell() -> Option<String> {
    let mut buf: [c_char; 1024] = [0; 1024];
    let mut entry: MaybeUninit<passwd> = MaybeUninit::uninit();
    let mut result: *mut passwd = ptr::null_mut();

    // SAFETY: `getuid` is always safe. `getpwuid_r` populates `entry`, using
    // `buf` as scratch storage, and sets `result` to point at `entry` on
    // success or to null when no entry exists. `entry` is read only once
    // `result` is non-null, and `buf` outlives the borrowed `pw_shell` string,
    // which is copied to an owned `String` before this returns.
    let uid = unsafe { libc::getuid() };
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }

    let entry = unsafe { entry.assume_init() };
    if entry.pw_shell.is_null() {
        return None;
    }

    let shell = unsafe { CStr::from_ptr(entry.pw_shell) };
    shell
        .to_str()
        .ok()
        .filter(|shell| !shell.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::{
        accumulate_loop, configure_child_env, dup_fd, dup_for_polling, fd_readable_now,
        prepend_path, push_tail, read_loop, shell_command, shell_or_default, spawn_writer,
        strip_escapes, wait_exit_status, AsRawFd, Child, CommandBuilder, ExitStatus, PtyOutput,
        PtySize, HANDOFF_SLICE, MULTIPLEXER_ENV_VARS, READ_BUF_SIZE,
    };
    use portable_pty::ChildKiller;
    use std::{
        ffi::{OsStr, OsString},
        io::{self, Cursor, Write},
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Condvar, Mutex,
        },
        thread,
        time::Duration,
    };

    fn shell(path: &str) -> Option<String> {
        Some(path.to_string())
    }

    #[test]
    fn shell_or_default_prefers_env_over_passwd() {
        assert_eq!(
            shell_or_default(shell("/bin/zsh"), shell("/bin/bash")),
            "/bin/zsh"
        );
    }

    #[test]
    fn shell_or_default_uses_passwd_when_env_unset_or_empty() {
        assert_eq!(shell_or_default(None, shell("/bin/bash")), "/bin/bash");
        assert_eq!(shell_or_default(shell(""), shell("/bin/bash")), "/bin/bash");
    }

    #[test]
    fn shell_or_default_falls_back_to_bin_sh() {
        assert_eq!(shell_or_default(None, None), "/bin/sh");
        assert_eq!(shell_or_default(shell(""), shell("")), "/bin/sh");
    }

    #[test]
    fn shell_command_sets_term_and_stoatty_env() {
        let command = shell_command(
            "/bin/sh",
            &[],
            None,
            Some(Path::new("/opt/stoat/bin")),
            Some("20260718-143022-99"),
            Some("/run/stoat/stoatty-win-42.sock"),
            "gruvbox-dark",
        );
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("STOATTY"), Some(OsStr::new("1")));
        assert_eq!(
            command.get_env("STOATTY_VERSION"),
            Some(OsStr::new(crate::cli::VERSION_INFO)),
        );
        assert_eq!(
            command.get_env("STOATTY_LOG_ID"),
            Some(OsStr::new("20260718-143022-99")),
        );
        assert_eq!(
            command.get_env("STOATTY_WINDOW_SOCKET"),
            Some(OsStr::new("/run/stoat/stoatty-win-42.sock")),
        );
        assert_eq!(
            command.get_env("STOAT_THEME"),
            Some(OsStr::new("gruvbox-dark")),
        );
        let path = command.get_env("PATH").expect("PATH set from stoat dir");
        assert!(
            path.to_str()
                .expect("PATH is utf8")
                .starts_with("/opt/stoat/bin"),
            "stoat dir prepended to PATH: {path:?}"
        );

        let without_id = shell_command("/bin/sh", &[], None, None, None, None, "");
        assert_eq!(without_id.get_env("STOATTY_LOG_ID"), None);
        assert_eq!(without_id.get_env("STOATTY_WINDOW_SOCKET"), None);
        assert_eq!(
            without_id.get_env("STOAT_THEME"),
            None,
            "an unnamed theme exports nothing, leaving the child on its own default"
        );
    }

    #[test]
    fn configure_child_env_strips_multiplexer_vars() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TMUX", "/tmp/tmux-1000/default,1,0");
        command.env("TMUX_PANE", "%3");
        command.env("ZELLIJ", "0");
        command.env("ZELLIJ_SESSION_NAME", "main");
        command.env("ZELLIJ_PANE_ID", "71");

        configure_child_env(&mut command, None, None, None, "");

        for var in MULTIPLEXER_ENV_VARS {
            assert_eq!(command.get_env(var), None, "{var} not stripped");
        }
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("STOATTY"), Some(OsStr::new("1")));
        assert_eq!(
            command.get_env("STOATTY_VERSION"),
            Some(OsStr::new(crate::cli::VERSION_INFO)),
        );
    }

    /// A stoatty launched from another terminal inherits that terminal's
    /// `TERM_PROGRAM`, so a child reads the wrong name unless this replaces it.
    #[test]
    fn configure_child_env_names_stoatty_as_the_terminal() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERM_PROGRAM", "iTerm.app");
        command.env("TERM_PROGRAM_VERSION", "3.4.19");

        configure_child_env(&mut command, None, None, None, "");

        assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("stoatty")));
        assert_eq!(
            command.get_env("TERM_PROGRAM_VERSION"),
            Some(OsStr::new(crate::cli::PACKAGE_VERSION)),
        );
        assert!(
            !crate::cli::PACKAGE_VERSION.contains(' '),
            "a consumer parses this as a version, so it carries no build detail",
        );
    }

    #[test]
    fn shell_command_sets_cwd_when_given() {
        let command = shell_command(
            "/bin/sh",
            &[],
            Some(Path::new("/tmp")),
            None,
            None,
            None,
            "",
        );
        assert_eq!(
            command.get_cwd().map(|cwd| cwd.as_os_str()),
            Some(OsStr::new("/tmp"))
        );
    }

    #[test]
    fn prepend_path_puts_dir_first() {
        assert_eq!(
            prepend_path(
                Path::new("/opt/stoat"),
                Some(OsString::from("/usr/bin:/bin"))
            ),
            OsString::from("/opt/stoat:/usr/bin:/bin")
        );
    }

    #[test]
    fn prepend_path_without_existing_is_just_dir() {
        assert_eq!(
            prepend_path(Path::new("/opt/stoat"), None),
            OsString::from("/opt/stoat")
        );
        assert_eq!(
            prepend_path(Path::new("/opt/stoat"), Some(OsString::new())),
            OsString::from("/opt/stoat")
        );
    }

    /// Drive [`accumulate_loop`] over `reads`, each entry one read's bytes, with
    /// `readable_now` answered from `pending`. Returns the chunks the sink saw
    /// and whether it ended with `Eof`.
    fn accumulated(reads: &[&[u8]], pending: &[bool]) -> (Vec<Vec<u8>>, bool) {
        let (chunks, eof) = accumulated_against(reads, pending, || true);
        (chunks.into_iter().map(|(bytes, _)| bytes).collect(), eof)
    }

    /// [`accumulated`] with a sink that decides per call whether to take the
    /// chunk. Each chunk is reported with whether that call was one it could
    /// have refused.
    fn accumulated_against(
        reads: &[&[u8]],
        pending: &[bool],
        mut take: impl FnMut() -> bool,
    ) -> (Vec<(Vec<u8>, bool)>, bool) {
        let mut next_read = 0;
        let mut next_pending = 0;
        let mut chunks: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut eof = false;

        accumulate_loop(
            |buf| {
                let Some(bytes) = reads.get(next_read) else {
                    return Ok(0);
                };
                next_read += 1;
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            },
            || {
                let answer = pending.get(next_pending).copied().unwrap_or(false);
                next_pending += 1;
                answer
            },
            |out| match out {
                PtyOutput::Data { bytes, may_refuse } => {
                    if may_refuse && !take() {
                        return false;
                    }
                    chunks.push((bytes.to_vec(), may_refuse));
                    true
                },
                PtyOutput::Eof => {
                    eof = true;
                    true
                },
            },
        );

        (chunks, eof)
    }

    #[test]
    fn reads_arriving_together_reach_the_sink_as_one_chunk() {
        // The point of the accumulation. Three reads with more already pending
        // after the first two cost one parse rather than three.
        let (chunks, eof) = accumulated(&[b"one", b"two", b"three"], &[true, true, false]);

        assert_eq!(chunks, vec![b"onetwothree".to_vec()]);
        assert!(eof, "ends with Eof");
    }

    #[test]
    fn a_lull_flushes_rather_than_waiting_for_more() {
        // Accumulating past what is already waiting would trade latency for
        // batching, so a read with nothing behind it goes on by itself.
        let (chunks, eof) = accumulated(&[b"one", b"two"], &[false, false]);

        assert_eq!(chunks, vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(eof, "ends with Eof");
    }

    /// A sink that refuses does not park the reader while output is still
    /// waiting. Draining the pty is what keeps the child running, so the bytes
    /// are held and the reader goes back for more.
    #[test]
    fn a_refused_chunk_is_held_while_there_is_more_to_read() {
        // The first hand-off is refused with more already pending, so the
        // reader takes that in too rather than waiting to be listened to.
        let calls = std::cell::Cell::new(0);
        let (chunks, eof) =
            accumulated_against(&[b"one", b"two", b"three"], &[false, true, false], || {
                calls.set(calls.get() + 1);
                calls.get() > 1
            });

        assert_eq!(
            chunks,
            vec![(b"onetwo".to_vec(), true), (b"three".to_vec(), true)],
            "the refused bytes come back with what was read while they waited",
        );
        assert!(eof, "ends with Eof");
    }

    /// Holding refused bytes is only safe while more is on its way. With
    /// nothing left to read the next read would park on a quiet child, and the
    /// bytes would sit there with nothing to release them, so the reader waits
    /// on the sink instead.
    #[test]
    fn a_refused_chunk_with_nothing_behind_it_is_handed_over_anyway() {
        // The read below stands in for one that would park on a quiet child.
        // Reaching it while bytes are still held is the stall itself, so the
        // reader has to have insisted before it gets there.
        let holding = std::cell::Cell::new(false);
        let mut reads = [b"only"].into_iter();
        let mut chunks: Vec<Vec<u8>> = Vec::new();

        accumulate_loop(
            |buf| {
                let Some(bytes) = reads.next() else {
                    assert!(!holding.get(), "went back to a read still holding output");
                    return Ok(0);
                };
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            },
            || false,
            |out| match out {
                PtyOutput::Data { bytes, may_refuse } => {
                    if may_refuse {
                        holding.set(true);
                        return false;
                    }
                    holding.set(false);
                    chunks.push(bytes.to_vec());
                    true
                },
                PtyOutput::Eof => true,
            },
        );

        assert_eq!(
            chunks,
            vec![b"only".to_vec()],
            "handed over on a call it could not refuse",
        );
    }

    #[test]
    fn a_full_buffer_flushes_mid_burst() {
        // Output still pending cannot hold the flush off forever, or the burst
        // would have no bound. The buffer's worth then goes out in sink-sized
        // calls, since one call is a stretch the main thread cannot render in.
        let flood = vec![b'x'; READ_BUF_SIZE];
        let (chunks, eof) = accumulated(&[&flood, b"tail"], &[true, true]);

        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![
                HANDOFF_SLICE,
                HANDOFF_SLICE,
                HANDOFF_SLICE,
                HANDOFF_SLICE,
                4
            ],
            "the buffer's worth in slices, then what came after it",
        );
        assert_eq!(
            chunks.concat(),
            [flood, b"tail".to_vec()].concat(),
            "every byte reaches the sink once, in order",
        );
        assert!(eof, "ends with Eof");
    }

    #[test]
    fn bytes_held_when_the_shell_closes_reach_the_sink_first() {
        // A burst interrupted by end of input still has to be handed on, or the
        // child's last output is lost with it.
        let (chunks, eof) = accumulated(&[b"parting"], &[true]);

        assert_eq!(chunks, vec![b"parting".to_vec()]);
        assert!(eof, "ends with Eof");
    }

    #[test]
    fn a_descriptor_reads_as_pending_only_once_something_is_waiting() {
        // What decides whether a burst keeps accumulating. Answering yes on an
        // empty descriptor would park the reader in a blocking read holding
        // bytes the sink has not seen.
        let (reader, mut writer) = io::pipe().expect("pipe");
        let fd = dup_fd(reader.as_raw_fd()).expect("dup a pipe end");

        assert!(!fd_readable_now(&fd), "nothing written yet");
        writer.write_all(b"x").expect("write");
        assert!(fd_readable_now(&fd), "a byte is waiting");
    }

    #[test]
    fn a_real_pty_master_hands_out_a_descriptor_to_poll() {
        // The batching hinges on this. A master that stopped exposing its
        // descriptor would drop the reader back to flushing per read silently,
        // with every other test still passing.
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        assert!(dup_for_polling(pair.master.as_ref()).is_some());
    }

    #[test]
    fn a_descriptor_the_kernel_rejects_leaves_nothing_to_poll() {
        // The degradation the accumulation leans on. With no descriptor to ask,
        // the reader hands every read on by itself, which is correct and merely
        // unbatched.
        assert!(dup_fd(-1).is_none(), "there is nothing to duplicate");
    }

    #[test]
    fn read_loop_chunks_at_the_buffer_boundary_then_signals_eof() {
        let data = vec![b'a'; READ_BUF_SIZE + 100];
        let mut sizes = Vec::new();
        let mut eof = false;
        read_loop(
            Cursor::new(data),
            || false,
            |out| match out {
                PtyOutput::Data { bytes, .. } => {
                    sizes.push(bytes.len());
                    true
                },
                PtyOutput::Eof => {
                    eof = true;
                    true
                },
            },
        );

        assert_eq!(
            sizes.iter().sum::<usize>(),
            READ_BUF_SIZE + 100,
            "every byte reaches the sink once"
        );
        assert!(
            sizes.iter().all(|size| *size <= HANDOFF_SLICE),
            "a buffer's worth goes out in sink-sized calls, not one: {sizes:?}",
        );
        assert_eq!(
            sizes.last(),
            Some(&100),
            "the remainder past the buffer is its own call"
        );
        assert!(eof, "ends with Eof");
    }

    /// An in-memory [`Write`] that parks every write on a gate until it opens,
    /// then records the bytes, standing in for a PTY master whose child has
    /// stopped draining its input.
    struct GatedWriter {
        gate: Arc<(Mutex<bool>, Condvar)>,
        written: Arc<(Mutex<Vec<u8>>, Condvar)>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (open_lock, open_cvar) = &*self.gate;
            let mut open = open_lock.lock().unwrap();
            while !*open {
                open = open_cvar.wait(open).unwrap();
            }
            drop(open);

            let (buf_lock, buf_cvar) = &*self.written;
            buf_lock.lock().unwrap().extend_from_slice(buf);
            buf_cvar.notify_all();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn spawn_writer_does_not_block_the_sender_on_a_parked_write() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let written = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let depth = Arc::new(AtomicUsize::new(0));
        let tx = spawn_writer(
            Box::new(GatedWriter {
                gate: gate.clone(),
                written: written.clone(),
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
    }

    #[test]
    #[ignore = "throughput benchmark; run with: cargo test -p stoatty --lib -- --ignored pty_read_throughput"]
    fn pty_read_throughput() {
        let total = 64 * 1024 * 1024;
        let (reader, mut writer) = io::pipe().unwrap();

        let feeder = thread::spawn(move || {
            let block = vec![b'x'; 64 * 1024];
            let mut left = total;
            while left > 0 {
                let n = block.len().min(left);
                if writer.write_all(&block[..n]).is_err() {
                    break;
                }
                left -= n;
            }
        });

        let mut chunks = 0usize;
        let mut bytes = 0usize;
        let start = std::time::Instant::now();
        read_loop(
            reader,
            || false,
            |out| {
                if let PtyOutput::Data { bytes: chunk, .. } = out {
                    chunks += 1;
                    bytes += chunk.len();
                }
                true
            },
        );
        let elapsed = start.elapsed();
        feeder.join().unwrap();

        let mb = bytes as f64 / (1024.0 * 1024.0);
        eprintln!(
            "pty read {mb:.0}MB: {chunks} chunks, {elapsed:?}, {:.0} MB/s",
            mb / elapsed.as_secs_f64()
        );
    }

    #[test]
    fn push_tail_retains_only_the_newest_cap_bytes() {
        let mut tail = Vec::new();
        push_tail(&mut tail, b"abc", 4);
        assert_eq!(tail, b"abc", "within cap, nothing is dropped");

        push_tail(&mut tail, b"defg", 4);
        assert_eq!(
            tail, b"defg",
            "overflow drops the oldest, keeping the newest cap"
        );

        push_tail(&mut tail, b"hijklmn", 4);
        assert_eq!(
            tail, b"klmn",
            "a chunk longer than the cap supersedes the tail, keeping its own newest cap"
        );

        push_tail(&mut tail, b"opqr", 4);
        assert_eq!(
            tail, b"opqr",
            "a chunk exactly at the cap replaces the tail entirely"
        );
    }

    #[test]
    fn strip_escapes_reduces_the_repro_sample_to_the_error_line() {
        let raw = "\x1b[?1049l\x1b[?1006l\x1b[?1002l\x1b[?1000l\
                   Error: requested fixture `rust-lsp` was not found\n";
        assert_eq!(
            strip_escapes(raw),
            "Error: requested fixture `rust-lsp` was not found"
        );
    }

    #[test]
    fn strip_escapes_drops_osc_and_sgr_but_keeps_text_and_newlines() {
        let raw = "\x1b]0;window title\x07\x1b[31mred line\x1b[0m\nplain line";
        assert_eq!(strip_escapes(raw), "red line\nplain line");
    }

    #[derive(Debug)]
    struct StubKiller;

    impl ChildKiller for StubKiller {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(StubKiller)
        }
    }

    /// A [`Child`] that reports "still running" for `polls_left` calls, then
    /// exits with `code`, so [`wait_exit_status`]'s polling loop runs
    /// deterministically without a real process.
    #[derive(Debug)]
    struct StubChild {
        polls_left: usize,
        code: u32,
    }

    impl ChildKiller for StubChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(StubKiller)
        }
    }

    impl Child for StubChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            if self.polls_left == 0 {
                return Ok(Some(ExitStatus::with_exit_code(self.code)));
            }
            self.polls_left -= 1;
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(self.code))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[test]
    fn wait_exit_status_returns_the_code_after_polling() {
        let mut child = StubChild {
            polls_left: 3,
            code: 3,
        };
        let status = wait_exit_status(&mut child, Duration::from_secs(1), Duration::from_millis(1));
        assert_eq!(status.map(|status| status.exit_code()), Some(3));
    }

    #[test]
    fn wait_exit_status_times_out_when_the_child_never_exits() {
        let mut child = StubChild {
            polls_left: usize::MAX,
            code: 0,
        };
        let status = wait_exit_status(
            &mut child,
            Duration::from_millis(5),
            Duration::from_millis(1),
        );
        assert!(status.is_none(), "no status resolved within the timeout");
    }
}
