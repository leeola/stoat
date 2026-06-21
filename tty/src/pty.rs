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
    ffi::{c_char, CStr},
    io::{self, Read, Write},
    mem::MaybeUninit,
    ptr,
    thread::{self, JoinHandle},
};

/// A chunk of PTY activity handed to the [`Pty::spawn`] sink.
pub(crate) enum PtyOutput {
    /// Bytes read from the shell, to feed into the parser.
    Data(Vec<u8>),
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
    /// start reading.
    ///
    /// `sink` runs on the reader thread: it is called with [`PtyOutput::Data`]
    /// for each chunk the shell writes and once with [`PtyOutput::Eof`] when the
    /// shell exits. It must be `Send` since it runs off the main thread.
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        rows: u16,
        cols: u16,
        sink: impl FnMut(PtyOutput) + Send + 'static,
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
            .spawn_command(shell_command(program, args))
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

/// Build the shell command with the stoatty environment.
///
/// `TERM` selects the terminfo the shell and its children load. `STOATTY` marks
/// the shell as running under stoatty, so a child can gate stoatty-only output on
/// its presence synchronously at startup, without the `XTVERSION` query round
/// trip.
fn shell_command(program: &str, args: &[String]) -> CommandBuilder {
    let mut command = CommandBuilder::new(program);
    command.args(args);
    command.env("TERM", "xterm-256color");
    command.env("STOATTY", "1");
    command
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
fn read_loop(mut reader: impl Read, mut sink: impl FnMut(PtyOutput)) {
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => sink(PtyOutput::Data(buf[..n].to_vec())),
        }
    }
    sink(PtyOutput::Eof);
}

/// The shell to launch, in order of preference: `$SHELL`, the passwd entry's
/// login shell, then `/bin/sh`.
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
    use super::{read_loop, shell_command, shell_or_default, PtyOutput, READ_BUF_SIZE};
    use std::{
        ffi::OsStr,
        io::{self, Cursor, Write},
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
        let command = shell_command("/bin/sh", &[]);
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("STOATTY"), Some(OsStr::new("1")));
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
