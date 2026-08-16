use std::{
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
};

/// Output captured from a single shell-host invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// Run a shell command. Production code routes through this trait so
/// tests can install [`crate::FakeShell`] instead of spawning real
/// subprocesses.
pub trait ShellHost: Send + Sync {
    /// Run `cmd` (interpreted by `sh -c`), feeding `stdin` to the
    /// command's stdin. Returns the captured stdout, stderr, and
    /// exit status. The exit code is `-1` when the process was
    /// terminated by a signal.
    ///
    /// `cwd` sets the child's working directory. `None` inherits the
    /// process cwd. `env` overrides the child's environment: each
    /// `(key, Some(value))` sets the variable, each `(key, None)`
    /// removes it. Entries not listed are inherited unchanged.
    fn run(
        &self,
        cmd: &str,
        stdin: &[u8],
        cwd: Option<&Path>,
        env: &[(String, Option<String>)],
    ) -> io::Result<ShellOutput>;
}

/// Production [`ShellHost`] backed by `std::process::Command` with
/// `sh -c`. Synchronous; the calling thread blocks until the command
/// exits.
pub struct LocalShell;

impl ShellHost for LocalShell {
    fn run(
        &self,
        cmd: &str,
        stdin: &[u8],
        cwd: Option<&Path>,
        env: &[(String, Option<String>)],
    ) -> io::Result<ShellOutput> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        for (key, value) in env {
            match value {
                Some(v) => command.env(key, v),
                None => command.env_remove(key),
            };
        }
        let mut child = command.spawn()?;

        // The write and the read run together. Both pipes hold a fixed kernel
        // buffer, so a command that fills its stdout before it has read all of
        // its stdin blocks, and a caller that writes everything first blocks
        // with it. Neither side then moves again.
        //
        // The thread takes the handle by value and so closes the pipe as it
        // ends, which is the EOF the child waits for. `wait_with_output` closes
        // its own copy, but the handle is gone from the child by then.
        let sin = child.stdin.take();
        let output = std::thread::scope(|scope| {
            // A command that stops reading early, `head -1` among them, breaks
            // this pipe. The command has not failed, so the error goes nowhere.
            scope.spawn(move || {
                if let Some(mut sin) = sin {
                    let _ = sin.write_all(stdin);
                }
            });
            child.wait_with_output()
        })?;
        Ok(ShellOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A megabyte round-trips, which is far past the kernel's pipe buffer.
    ///
    /// A host that writes all of stdin before it reads any output hangs here
    /// rather than fails, since the hang is the defect itself. A payload small
    /// enough to fit the buffer passes either way and pins nothing.
    #[test]
    fn a_megabyte_round_trips_through_cat() {
        let payload = "x".repeat(1024 * 1024);
        let out = LocalShell
            .run("cat", payload.as_bytes(), None, &[])
            .expect("cat runs");

        assert_eq!(out.stdout.len(), payload.len());
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.is_empty(), "cat says nothing on stderr");
    }

    /// A command that stops reading breaks the stdin pipe, and that is the
    /// command at work rather than a failure to report.
    #[test]
    fn a_command_that_stops_reading_still_reports_its_output() {
        let payload = "line\n".repeat(200_000);
        let out = LocalShell
            .run("head -1", payload.as_bytes(), None, &[])
            .expect("head runs");

        assert_eq!(out.stdout, b"line\n");
        assert_eq!(out.exit_code, 0);
    }
}
