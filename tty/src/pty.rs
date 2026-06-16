//! The PTY shell: spawns a child shell over a pseudoterminal and pumps its
//! output to the app on a dedicated reader thread.
//!
//! Lives in the app crate, off the terminal-core crates, so [`stoatty_term`]
//! stays a pure bytes-to-grid model. The reader thread feeds bytes back through
//! a caller-supplied sink, which the app forwards onto the winit event loop, so
//! a blocking PTY read never stalls rendering.

use portable_pty::{self, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    io::{self, Read},
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
/// Owns the PTY master (for resizing), the child handle, and the reader thread.
/// Dropping it kills the shell, which closes the slave and ends the reader
/// thread, so closing the window tears the shell down.
pub(crate) struct Pty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    _reader: JoinHandle<()>,
}

impl Pty {
    /// Spawn `shell` over a fresh PTY sized `rows` by `cols` and start reading.
    ///
    /// `sink` runs on the reader thread: it is called with [`PtyOutput::Data`]
    /// for each chunk the shell writes and once with [`PtyOutput::Eof`] when the
    /// shell exits. It must be `Send` since it runs off the main thread.
    pub(crate) fn spawn(
        shell: &str,
        rows: u16,
        cols: u16,
        mut sink: impl FnMut(PtyOutput) + Send + 'static,
    ) -> io::Result<Pty> {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;

        let mut command = CommandBuilder::new(shell);
        command.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;

        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink(PtyOutput::Data(buf[..n].to_vec())),
                }
            }
            sink(PtyOutput::Eof);
        });

        Ok(Pty {
            master: pair.master,
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
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// The shell to launch: `$SHELL`, or `/bin/sh` when it is unset or empty.
pub(crate) fn default_shell() -> String {
    shell_or_default(std::env::var("SHELL").ok())
}

fn shell_or_default(shell: Option<String>) -> String {
    shell
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

#[cfg(test)]
mod tests {
    use super::shell_or_default;

    #[test]
    fn shell_or_default_prefers_env_value() {
        assert_eq!(shell_or_default(Some("/bin/zsh".to_string())), "/bin/zsh");
    }

    #[test]
    fn shell_or_default_falls_back_when_unset_or_empty() {
        assert_eq!(shell_or_default(None), "/bin/sh");
        assert_eq!(shell_or_default(Some(String::new())), "/bin/sh");
    }
}
