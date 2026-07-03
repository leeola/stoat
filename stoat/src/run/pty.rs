use super::RunId;
use crate::{
    host::terminal::{open_local_pty, SpawnArgs, TerminalHost, TerminalSession},
    term_session::TermId,
    workspace::WorkspaceUid,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_scheduler::Executor;
use tokio::sync::mpsc;

pub enum PtyNotification {
    Output {
        run_id: RunId,
        data: Vec<u8>,
    },
    CommandDone {
        run_id: RunId,
        exit_status: Option<i32>,
    },
    TermOutput {
        agent_id: TermId,
        data: Vec<u8>,
    },
    TermExited {
        term_id: TermId,
    },
}

pub struct ShellHandle {
    session: Arc<dyn TerminalSession>,
}

impl ShellHandle {
    pub(crate) fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self { session }
    }

    pub fn send_command(&self, command: &str) {
        use futures::FutureExt;
        let payload = format!("{command}\n");
        let _ = self.session.write(payload.as_bytes()).now_or_never();
    }

    pub fn send_interrupt(&self) {
        use futures::FutureExt;
        let _ = self.session.write(b"\x03").now_or_never();
    }

    pub fn kill(&self) {
        use futures::FutureExt;
        let _ = self.session.kill().now_or_never();
    }
}

pub fn spawn_shell(
    host: &dyn TerminalHost,
    executor: &Executor,
    cwd: &Path,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    use futures::FutureExt;

    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["--noediting".into(), "--noprofile".into(), "--norc".into()],
        env: vec![
            ("PS1".into(), String::new()),
            ("PS2".into(), String::new()),
            // PS0 emits the OSC 133 command-start mark before each command
            // runs. PROMPT_COMMAND emits the done mark with the exit code and
            // an OSC 7 cwd report after it. The run pane reads these to bound
            // output blocks and track the shell's working directory.
            ("PS0".into(), "\x1b]133;C\x07".into()),
            (
                "PROMPT_COMMAND".into(),
                "printf '\x1b]133;D;%s\x07\x1b]7;file://%s\x07' \"$?\" \"$PWD\"".into(),
            ),
            ("TERM".into(), "dumb".into()),
        ],
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    let session = match host.spawn(args).now_or_never() {
        Some(result) => result?,
        None => {
            return Err(std::io::Error::other(
                "terminal host did not spawn the run shell synchronously",
            ))
        },
    };
    let session: Arc<dyn TerminalSession> = Arc::from(session);
    executor
        .spawn(reader_task(session.clone(), run_id, pty_tx))
        .detach();

    // bash --noediting echoes injected lines back into the grid. Turn the
    // tty's echo off so only real command output is rendered.
    let _ = session.write(b"stty -echo\n").now_or_never();

    Ok(ShellHandle::new(session))
}

pub fn spawn_oneshot(
    executor: &Executor,
    command: &str,
    cwd: &Path,
    width: u16,
    pty_tx: mpsc::Sender<PtyNotification>,
    run_id: RunId,
) -> std::io::Result<ShellHandle> {
    let args = SpawnArgs {
        program: "bash".into(),
        args: vec!["-c".into(), command.to_string()],
        env: vec![("TERM".into(), "dumb".into())],
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    let session: Arc<dyn TerminalSession> = Arc::new(open_local_pty(args)?);
    executor
        .spawn(reader_task(session.clone(), run_id, pty_tx))
        .detach();

    Ok(ShellHandle::new(session))
}

/// Spawn `claude` as an owned subshell keyed to the workspace `uid`,
/// returning its [`TerminalSession`].
///
/// The caller owns the returned session, and dropping it closes the PTY.
/// The child's env carries `STOAT_SESSION` (the uid) and `STOAT_AGENT_SOCK`
/// (the [`agent_socket_path`]) so a hook callback resolves which session
/// and socket to reach.
pub async fn spawn_claude(
    host: &dyn TerminalHost,
    uid: WorkspaceUid,
    cwd: &Path,
) -> std::io::Result<Box<dyn TerminalSession>> {
    let socket_path = agent_socket_path(uid)?;
    let editor_command = editor_bridge_command();
    host.spawn(claude_spawn_args(uid, cwd, &socket_path, &editor_command))
        .await
}

/// Spawn `program` as an owned subshell terminal session, returning its
/// [`TerminalSession`].
///
/// The caller owns the returned session, and dropping it closes the PTY. The
/// child runs with `TERM=xterm-256color` to match the xterm-compatible
/// emulator the pane renders into, and inherits no other environment beyond
/// the parent's.
pub async fn spawn_terminal(
    host: &dyn TerminalHost,
    cwd: &Path,
    program: &str,
    args: &[String],
) -> std::io::Result<Box<dyn TerminalSession>> {
    host.spawn(SpawnArgs {
        program: program.to_string(),
        args: args.to_vec(),
        env: vec![("TERM".into(), "xterm-256color".into())],
        cwd: cwd.to_path_buf(),
        width: 80,
        rows: 24,
    })
    .await
}

/// Spawn the reader that pumps a term session's PTY output into its
/// emulator, tagging each chunk with `agent_id`. Detached on the executor like
/// the run pane's reader.
pub fn spawn_term_reader(
    executor: &Executor,
    session: Arc<dyn TerminalSession>,
    agent_id: TermId,
    pty_tx: mpsc::Sender<PtyNotification>,
) {
    executor
        .spawn(term_reader_task(session, agent_id, pty_tx))
        .detach();
}

async fn term_reader_task(
    session: Arc<dyn TerminalSession>,
    agent_id: TermId,
    tx: mpsc::Sender<PtyNotification>,
) {
    loop {
        let chunk = match session.read_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        if tx
            .send(PtyNotification::TermOutput {
                agent_id,
                data: chunk,
            })
            .await
            .is_err()
        {
            break;
        }
    }

    // The read loop ends when the child closes its PTY end (shell exit) or on a
    // read error. Signal the exit unconditionally so the app can retire a
    // terminal pane. The handler decides what to do by pane kind, and a closed
    // channel drops this send silently.
    let _ = tx
        .send(PtyNotification::TermExited { term_id: agent_id })
        .await;
}

/// Filesystem path of the per-session agent hook socket for `uid`, under
/// the Stoat state dir.
///
/// Passed to the owned Claude subshell as `STOAT_AGENT_SOCK`. The
/// in-process IPC server binds the same path, so a hook callback reaches
/// the owning session.
pub fn agent_socket_path(uid: WorkspaceUid) -> std::io::Result<PathBuf> {
    Ok(stoat_log::state_dir()?.join(format!("agent-{uid}.sock")))
}

fn claude_spawn_args(
    uid: WorkspaceUid,
    cwd: &Path,
    socket_path: &Path,
    editor_command: &str,
) -> SpawnArgs {
    SpawnArgs {
        program: "claude".into(),
        args: Vec::new(),
        env: vec![
            ("STOAT_SESSION".into(), uid.to_string()),
            (
                "STOAT_AGENT_SOCK".into(),
                socket_path.to_string_lossy().into_owned(),
            ),
            ("EDITOR".into(), editor_command.to_string()),
            ("VISUAL".into(), editor_command.to_string()),
        ],
        cwd: cwd.to_path_buf(),
        width: 80,
        rows: 24,
    }
}

/// The `$EDITOR` command the owned agent runs to compose prompts in the IDE.
///
/// Resolves the current executable so the agent invokes this same binary's
/// `editor` subcommand even when a bare `stoat` is not on its PATH. The
/// subcommand reads the socket from the already-injected `STOAT_AGENT_SOCK`, so
/// no further arguments are baked in.
fn editor_bridge_command() -> String {
    editor_command_for(std::env::current_exe().ok().as_deref())
}

/// Format the editor command from a resolved executable path.
///
/// Falls back to a bare `stoat editor` when the path is unknown, relying on
/// PATH resolution in that case.
fn editor_command_for(exe: Option<&Path>) -> String {
    match exe {
        Some(exe) => format!("{} editor", exe.to_string_lossy()),
        None => "stoat editor".to_string(),
    }
}

async fn reader_task(
    session: Arc<dyn TerminalSession>,
    run_id: RunId,
    tx: mpsc::Sender<PtyNotification>,
) {
    loop {
        let chunk = match session.read_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        if tx
            .send(PtyNotification::Output {
                run_id,
                data: chunk,
            })
            .await
            .is_err()
        {
            break;
        }
    }

    // The read loop ends when the shell closes its PTY end (exit) or on a read
    // error. Signal completion at EOF so a still-running block finalizes even
    // without an OSC 133 done mark, which the oneshot modal path relies on.
    let _ = tx
        .send(PtyNotification::CommandDone {
            run_id,
            exit_status: None,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_args_inject_session_socket_and_editor_env() {
        let uid = WorkspaceUid(0xABCD);
        let args = claude_spawn_args(
            uid,
            Path::new("/work"),
            Path::new("/run/agent.sock"),
            "/usr/bin/stoat editor",
        );
        assert_eq!(args.program, "claude");
        assert_eq!(args.cwd, Path::new("/work"));
        assert_eq!(
            args.env,
            vec![
                ("STOAT_SESSION".to_string(), uid.to_string()),
                (
                    "STOAT_AGENT_SOCK".to_string(),
                    "/run/agent.sock".to_string()
                ),
                ("EDITOR".to_string(), "/usr/bin/stoat editor".to_string()),
                ("VISUAL".to_string(), "/usr/bin/stoat editor".to_string()),
            ],
        );
    }

    #[test]
    fn editor_command_uses_exe_path_with_fallback() {
        assert_eq!(
            editor_command_for(Some(Path::new("/usr/bin/stoat"))),
            "/usr/bin/stoat editor"
        );
        assert_eq!(editor_command_for(None), "stoat editor");
    }
}
