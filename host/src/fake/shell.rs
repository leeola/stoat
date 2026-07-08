use crate::shell::{ShellHost, ShellOutput};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// In-memory [`ShellHost`] used by tests. Calls to
/// [`ShellHost::run`] look up the command string in a programmed
/// response table; unprogrammed commands return an empty stdout
/// with exit code 0.
#[derive(Default)]
pub struct FakeShell {
    responses: Mutex<HashMap<String, ShellOutput>>,
    invocations: Mutex<Vec<FakeShellInvocation>>,
}

/// One captured call to [`FakeShell::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeShellInvocation {
    pub cmd: String,
    pub stdin: Vec<u8>,
    /// The working directory the caller requested, or `None` for the
    /// process cwd.
    pub cwd: Option<PathBuf>,
    /// The env overrides the caller passed, in call order.
    pub env: Vec<(String, Option<String>)>,
}

impl FakeShell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Programme `output` as the response when `cmd` is run.
    /// Subsequent runs of the same command keep yielding the same
    /// output. Re-programming with the same key replaces the prior
    /// entry.
    pub fn set_response(&self, cmd: impl Into<String>, output: ShellOutput) {
        self.responses
            .lock()
            .expect("FakeShell responses poisoned")
            .insert(cmd.into(), output);
    }

    /// Captured invocations in call order.
    pub fn invocations(&self) -> Vec<FakeShellInvocation> {
        self.invocations
            .lock()
            .expect("FakeShell invocations poisoned")
            .clone()
    }
}

impl ShellHost for FakeShell {
    fn run(
        &self,
        cmd: &str,
        stdin: &[u8],
        cwd: Option<&Path>,
        env: &[(String, Option<String>)],
    ) -> io::Result<ShellOutput> {
        self.invocations
            .lock()
            .expect("FakeShell invocations poisoned")
            .push(FakeShellInvocation {
                cmd: cmd.to_string(),
                stdin: stdin.to_vec(),
                cwd: cwd.map(Path::to_path_buf),
                env: env.to_vec(),
            });
        let responses = self.responses.lock().expect("FakeShell responses poisoned");
        Ok(responses.get(cmd).cloned().unwrap_or(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_records_cwd_and_env() {
        let shell = FakeShell::new();
        let env = vec![
            ("FOO".to_string(), Some("bar".to_string())),
            ("BAZ".to_string(), None),
        ];
        shell
            .run("cmd", b"in", Some(Path::new("/work")), &env)
            .unwrap();
        assert_eq!(
            shell.invocations(),
            vec![FakeShellInvocation {
                cmd: "cmd".to_string(),
                stdin: b"in".to_vec(),
                cwd: Some(PathBuf::from("/work")),
                env,
            }]
        );
    }
}
