//! Launch the `claude-acp` adapter as a subprocess and connect to it
//! over the piped-stdio JSON-RPC transport.

use crate::{AcpConnection, AcpError, ClientHandlers};
use snafu::{Location, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use stoat_agent_claude_code::jsonrpc::{JsonRpcPeer, SpawnError};
use stoat_scheduler::Executor;
use tokio::process::Command;

/// The npm package whose `bin` speaks ACP over stdio.
const CLAUDE_ACP_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";

/// Failure modes of [`launch`].
#[derive(Debug, Snafu)]
pub enum LaunchError {
    #[snafu(display("failed to spawn the ACP agent process"))]
    Spawn {
        source: SpawnError,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("failed to connect to the ACP agent"))]
    Connect {
        source: AcpError,
        #[snafu(implicit)]
        location: Location,
    },
}

/// Resolve, spawn, and connect to the `claude-acp` adapter. The adapter
/// runs in `cwd` with the inherited environment; the returned connection
/// has completed the ACP initialize handshake and answers agent requests
/// through `handlers`.
pub async fn launch(
    handlers: ClientHandlers,
    executor: Executor,
    cwd: PathBuf,
) -> Result<AcpConnection, LaunchError> {
    launch_command(resolve_command(&cwd), handlers, executor, cwd).await
}

/// Build the `npm exec` command that runs the `claude-acp` adapter in
/// `cwd`.
fn resolve_command(cwd: &Path) -> Command {
    let mut command = Command::new("npm");
    command
        .args(["exec", "--yes", "--", CLAUDE_ACP_PACKAGE])
        .current_dir(cwd);
    command
}

/// Spawn `command`'s piped stdio as the ACP transport and run the
/// initialize handshake. Separated from [`launch`] so tests can inject a
/// fake-agent command.
async fn launch_command(
    command: Command,
    handlers: ClientHandlers,
    executor: Executor,
    cwd: PathBuf,
) -> Result<AcpConnection, LaunchError> {
    let (peer, incoming) =
        JsonRpcPeer::spawn(command, &executor, None, None).context(SpawnSnafu)?;
    AcpConnection::connect(
        peer,
        incoming,
        handlers,
        executor,
        cwd.to_string_lossy().into_owned(),
    )
    .await
    .context(ConnectSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientHandlers;
    use std::sync::Arc;
    use stoat::host::{FakeFs, FakeTerminalHost, FakeTerminalSession};
    use stoat_scheduler::TokioScheduler;
    use tokio::sync::mpsc;

    /// A minimal ACP server: read the initialize request, echo its id in a
    /// result, and stay alive so the connection's stdio stays open.
    const FAKE_ACP_SERVER: &str = r#"read line; id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id"; sleep 5"#;

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    fn fake_handlers() -> ClientHandlers {
        ClientHandlers {
            fs: Arc::new(FakeFs::new()),
            permission_tx: mpsc::channel(1).0,
            terminal_host: Arc::new(FakeTerminalHost::new(Arc::new(FakeTerminalSession::new()))),
        }
    }

    #[test]
    fn resolve_command_runs_claude_acp_via_npm() {
        let command = resolve_command(Path::new("/project"));
        let std = command.as_std();
        assert_eq!(std.get_program(), "npm");
        assert_eq!(
            std.get_args().collect::<Vec<_>>(),
            ["exec", "--yes", "--", CLAUDE_ACP_PACKAGE]
        );
        assert_eq!(std.get_current_dir(), Some(Path::new("/project")));
    }

    #[tokio::test]
    async fn launch_command_connects_over_spawned_stdio() {
        let executor = executor();
        let mut command = Command::new("sh");
        command.args(["-c", FAKE_ACP_SERVER]);

        let connection = launch_command(command, fake_handlers(), executor, PathBuf::from("/work"))
            .await
            .expect("launch connects after the initialize handshake");
        drop(connection);
    }
}
