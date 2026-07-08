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
        if let Some(mut sin) = child.stdin.take() {
            sin.write_all(stdin)?;
        }
        let output = child.wait_with_output()?;
        Ok(ShellOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}
