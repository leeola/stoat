//! The PTY shell: spawns a child shell over a pseudoterminal and pumps its
//! output to the app on a dedicated reader thread.
//!
//! Lives in the app crate, off the terminal-core crates, so [`stoatty_term`]
//! stays a pure bytes-to-grid model. The reader thread feeds bytes back through
//! a caller-supplied sink, which the app forwards onto the winit event loop, so
//! a blocking PTY read never stalls rendering.

use libc::passwd;
use portable_pty::{self, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    ffi::{c_char, CStr, OsString},
    io::{self, Read, Write},
    mem::MaybeUninit,
    path::Path,
    ptr,
    thread::{self, JoinHandle},
};

/// A chunk of PTY activity handed to the [`Pty::spawn`] sink.
///
/// [`PtyOutput::Data`] borrows the reader thread's read buffer, so it is valid
/// only for the duration of the sink call. The sink must consume it before
/// returning and must not retain the slice, since the next read overwrites the
/// buffer.
pub(crate) enum PtyOutput<'a> {
    /// Bytes read from the shell, borrowed from the reader buffer, to feed into
    /// the parser.
    Data(&'a [u8]),
    /// The shell closed its end; no more data will follow.
    Eof,
}

/// A running shell attached to a pseudoterminal.
///
/// Owns the PTY master (for resizing), the writer to the shell's input (for
/// forwarding key presses), the child handle, and the reader thread. Dropping
/// it kills the shell, which closes the slave and ends the reader thread, so
/// closing the window tears the shell down.
pub(crate) struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _reader: JoinHandle<()>,
}

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
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        stoat_dir: Option<&Path>,
        rows: u16,
        cols: u16,
        sink: impl FnMut(PtyOutput<'_>) + Send + 'static,
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
            .spawn_command(shell_command(program, args, cwd, stoat_dir))
            .map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;

        let reader_thread = thread::spawn(move || read_loop(reader, sink));

        Ok(Pty {
            master: pair.master,
            writer,
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

    /// Write `bytes` to the shell's input, flushing so the shell sees them
    /// promptly.
    ///
    /// Encoded key presses flow through here, so typing in the window reaches
    /// the shell.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Build the shell command to launch under stoatty, running in `cwd` when
/// given. [`configure_child_env`] sets the environment the child inherits.
fn shell_command(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    stoat_dir: Option<&Path>,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.cwd(dir);
    }
    configure_child_env(&mut command, stoat_dir);
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
/// `TERM` selects the terminfo the shell and its children load. `STOATTY` marks
/// the shell as running under stoatty, so a child can gate stoatty-only output
/// on its presence synchronously at startup, without the `XTVERSION` query round
/// trip.
///
/// Inherited multiplexer markers ([`MULTIPLEXER_ENV_VARS`]) are removed because
/// stoatty presents a fresh terminal that owns and forwards its own window's
/// mouse, so a program inside it must detect stoatty and enable mouse
/// reporting rather than stand down for an outer `tmux`/`zellij` that does not
/// control this window. A real multiplexer launched inside stoatty re-sets
/// these for its own children, so in-stoatty-mux detection still stands down as
/// intended.
fn configure_child_env(command: &mut CommandBuilder, stoat_dir: Option<&Path>) {
    command.env("TERM", "xterm-256color");
    command.env("STOATTY", "1");
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

/// Size of the reader thread's buffer. Each read fills up to this many bytes
/// before a chunk is handed on, so a larger buffer means proportionally fewer
/// reads, allocations, and sink calls under firehose output. Sized in tens of
/// KiB to batch a `yes`/`cat` flood without large per-chunk allocations or
/// latency, and to stay well within the thread stack.
const READ_BUF_SIZE: usize = 64 * 1024;

/// Pump `reader` to `sink` until end of input: read into a reused buffer and
/// hand each fill on as a [`PtyOutput::Data`] chunk, then one [`PtyOutput::Eof`]
/// once the shell closes its end or the read errors.
fn read_loop(mut reader: impl Read, mut sink: impl FnMut(PtyOutput<'_>)) {
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => sink(PtyOutput::Data(&buf[..n])),
        }
    }
    sink(PtyOutput::Eof);
}

/// The shell to launch, in order of preference: `$SHELL`, the passwd entry's
/// login shell, then `/bin/sh`.
///
/// Used for the `--terminal` opt-out, which runs the login shell instead of the
/// stoat editor.
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
        configure_child_env, prepend_path, read_loop, shell_command, shell_or_default,
        CommandBuilder, PtyOutput, MULTIPLEXER_ENV_VARS, READ_BUF_SIZE,
    };
    use std::{
        ffi::{OsStr, OsString},
        io::{self, Cursor, Write},
        path::Path,
        thread,
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
        let command = shell_command("/bin/sh", &[], None, Some(Path::new("/opt/stoat/bin")));
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("STOATTY"), Some(OsStr::new("1")));
        let path = command.get_env("PATH").expect("PATH set from stoat dir");
        assert!(
            path.to_str()
                .expect("PATH is utf8")
                .starts_with("/opt/stoat/bin"),
            "stoat dir prepended to PATH: {path:?}"
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

        configure_child_env(&mut command, None);

        for var in MULTIPLEXER_ENV_VARS {
            assert_eq!(command.get_env(var), None, "{var} not stripped");
        }
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("STOATTY"), Some(OsStr::new("1")));
    }

    #[test]
    fn shell_command_sets_cwd_when_given() {
        let command = shell_command("/bin/sh", &[], Some(Path::new("/tmp")), None);
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

    #[test]
    fn read_loop_chunks_at_the_buffer_boundary_then_signals_eof() {
        let data = vec![b'a'; READ_BUF_SIZE + 100];
        let mut sizes = Vec::new();
        let mut eof = false;
        read_loop(Cursor::new(data), |out| match out {
            PtyOutput::Data(chunk) => sizes.push(chunk.len()),
            PtyOutput::Eof => eof = true,
        });

        assert_eq!(
            sizes,
            vec![READ_BUF_SIZE, 100],
            "a full buffer, then the remainder"
        );
        assert!(eof, "ends with Eof");
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
        read_loop(reader, |out| {
            if let PtyOutput::Data(chunk) = out {
                chunks += 1;
                bytes += chunk.len();
            }
        });
        let elapsed = start.elapsed();
        feeder.join().unwrap();

        let mb = bytes as f64 / (1024.0 * 1024.0);
        eprintln!(
            "pty read {mb:.0}MB: {chunks} chunks, {elapsed:?}, {:.0} MB/s",
            mb / elapsed.as_secs_f64()
        );
    }
}
