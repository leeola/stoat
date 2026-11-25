use super::CmdId;
use crate::{
    host::terminal::{merge_env_diff, SpawnArgs, TerminalHost, TerminalSession},
    term_session::TermId,
    workspace::{WorkspaceId, WorkspaceUid},
};
use futures::FutureExt;
use std::{
    io::Error,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use stoat_scheduler::Executor;
use tokio::sync::mpsc;

/// How many times the reader of a Cmd asks for the exit status after the PTY's
/// end, [`EXIT_STATUS_POLL`] apart.
const EXIT_STATUS_TRIES: u32 = 50;

/// The wait between two asks for the exit status of a Cmd's process.
const EXIT_STATUS_POLL: Duration = Duration::from_millis(10);

/// What a PTY reader tells the run loop.
///
/// A [`CmdId`] or [`TermId`] is a key local to the workspace that owns the
/// session, so the first sessions of two workspaces share a key. Each session
/// notification therefore carries the [`WorkspaceId`] of its owner. The run
/// loop applies it to that workspace, on screen or not.
pub enum PtyNotification {
    /// A chunk of the output of the run `run` of the Cmd `cmd`.
    CmdOutput {
        workspace: WorkspaceId,
        cmd: CmdId,
        run: u32,
        data: Vec<u8>,
    },
    /// The end of the run `run` of the Cmd `cmd`, with the exit status of its
    /// process, or `None` when no status came.
    CmdExited {
        workspace: WorkspaceId,
        cmd: CmdId,
        run: u32,
        exit_status: Option<i32>,
    },
    TermOutput {
        workspace: WorkspaceId,
        agent_id: TermId,
        data: Vec<u8>,
    },
    TermExited {
        workspace: WorkspaceId,
        term_id: TermId,
    },
    /// One chunk of a remote `:ssh` session's output, on its way to stdout as
    /// it is. Untagged because one session runs at a time.
    SshOutput {
        data: Vec<u8>,
    },
    SshExited {
        exit_status: Option<i32>,
    },
}

impl PtyNotification {
    /// Bytes of terminal output this notification carries, or 0 for one that
    /// carries none.
    ///
    /// The run loop parses this many bytes when it handles the notification, so
    /// it is what a per-turn parse budget counts against.
    pub(crate) fn payload_len(&self) -> usize {
        match self {
            PtyNotification::CmdOutput { data, .. }
            | PtyNotification::TermOutput { data, .. }
            | PtyNotification::SshOutput { data } => data.len(),
            PtyNotification::CmdExited { .. }
            | PtyNotification::TermExited { .. }
            | PtyNotification::SshExited { .. } => 0,
        }
    }
}

/// The process of one run of a Cmd, a `bash -c` of its line on a PTY.
pub(crate) struct CmdProcess {
    session: Arc<dyn TerminalSession>,
}

impl CmdProcess {
    pub(crate) fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self { session }
    }

    /// Write Ctrl-C into the process, as a terminal does for the key.
    pub(crate) fn send_interrupt(&self) {
        let _ = self.session.write(b"\x03").now_or_never();
    }

    pub(crate) fn kill(&self) {
        let _ = self.session.kill().now_or_never();
    }
}

/// Where the reader of one run of a Cmd sends what it reads.
///
/// A [`CmdId`] names a Cmd only within its own workspace, so each
/// notification carries the owning [`WorkspaceId`] too. The run number lets
/// the run loop drop what an earlier run of the Cmd sends after a later run
/// started.
pub(crate) struct CmdSink {
    pub(crate) tx: mpsc::Sender<PtyNotification>,
    pub(crate) workspace: WorkspaceId,
    pub(crate) cmd: CmdId,
    pub(crate) run: u32,
}

/// Start `line` as its own `bash -c` process on a PTY through `host`, in
/// `cwd`, and spawn its reader on `executor`.
///
/// The process sees the environment with `diff` over it and `TERM=dumb`. The
/// reader sends each chunk of output to `sink`, and the exit status at the
/// PTY's end.
pub(crate) fn spawn_cmd(
    host: &dyn TerminalHost,
    executor: &Executor,
    line: &str,
    cwd: &Path,
    width: u16,
    sink: CmdSink,
    diff: &[(String, Option<String>)],
) -> std::io::Result<CmdProcess> {
    let mut args = SpawnArgs {
        program: "bash".into(),
        args: vec!["-c".into(), line.to_owned()],
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    merge_env_diff(&mut args, diff);
    args.env.push(("TERM".into(), "dumb".into()));
    let session = match host.spawn(args).now_or_never() {
        Some(result) => result?,
        None => {
            return Err(Error::other(
                "terminal host did not spawn the command synchronously",
            ))
        },
    };
    let session: Arc<dyn TerminalSession> = Arc::from(session);
    executor
        .spawn(cmd_reader_task(session.clone(), sink, executor.clone()))
        .detach();
    Ok(CmdProcess::new(session))
}

/// Spawn `claude` as an owned subshell keyed to the workspace `uid`,
/// returning its [`TerminalSession`].
///
/// The caller owns the returned session, and dropping it closes the PTY.
///
/// With `socket_path`, the child's env also carries `STOAT_SESSION` (the uid)
/// and `STOAT_AGENT_SOCK` (that path), so a hook callback resolves which
/// session and socket to reach. Carried in rather than resolved here, because
/// the socket directory is a per-instance knob
/// ([`crate::Stoat::set_agent_socket_dir`]).
pub async fn spawn_claude(
    host: &dyn TerminalHost,
    uid: WorkspaceUid,
    cwd: &Path,
    diff: &[(String, Option<String>)],
    socket_path: Option<&Path>,
) -> std::io::Result<Box<dyn TerminalSession>> {
    let editor_command = editor_bridge_command();
    host.spawn(claude_spawn_args(
        uid,
        cwd,
        socket_path,
        &editor_command,
        diff,
    ))
    .await
}

/// The owning instance a terminal shell reaches back to, as the shell's
/// environment names it.
///
/// Carried into the spawn rather than resolved inside it, because the token is
/// minted before the PTY opens and the socket directory is a per-instance knob
/// ([`crate::Stoat::set_agent_socket_dir`]).
pub struct TermSpawnEnv {
    pub uid: WorkspaceUid,
    pub socket_path: PathBuf,
    pub token: u64,
}

/// Spawn `program` as an owned subshell terminal session, returning its
/// [`TerminalSession`].
///
/// The caller owns the returned session, and dropping it closes the PTY. The
/// child runs with `TERM=xterm-256color` to match the xterm-compatible
/// emulator the pane renders into, and inherits no other environment beyond
/// the parent's.
///
/// With `session_env`, the child also learns which instance, socket, and
/// terminal pane it belongs to, so a command run inside it addresses the
/// editor that hosts it. `EDITOR` and `VISUAL` stay untouched either way. A
/// terminal shell is the user's own, and an owned agent's blocking-editor
/// contract is not theirs.
pub async fn spawn_terminal(
    host: &dyn TerminalHost,
    cwd: &Path,
    program: &str,
    args: &[String],
    diff: &[(String, Option<String>)],
    session_env: Option<TermSpawnEnv>,
) -> std::io::Result<Box<dyn TerminalSession>> {
    host.spawn(terminal_spawn_args(
        cwd,
        program,
        args,
        diff,
        session_env.as_ref(),
    ))
    .await
}

fn terminal_spawn_args(
    cwd: &Path,
    program: &str,
    args: &[String],
    diff: &[(String, Option<String>)],
    session_env: Option<&TermSpawnEnv>,
) -> SpawnArgs {
    let mut spawn_args = SpawnArgs {
        program: program.to_string(),
        args: args.to_vec(),
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        width: 80,
        rows: 24,
    };
    merge_env_diff(&mut spawn_args, diff);
    spawn_args
        .env
        .push(("TERM".into(), "xterm-256color".into()));

    if let Some(env) = session_env {
        spawn_args.env.extend([
            ("STOAT_SESSION".into(), env.uid.to_string()),
            (
                "STOAT_AGENT_SOCK".into(),
                env.socket_path.to_string_lossy().into_owned(),
            ),
            ("STOAT_TERM_ID".into(), env.token.to_string()),
        ]);
    }
    spawn_args
}

/// Spawn the reader that pumps a term session's PTY output into its
/// emulator, tagging each chunk with `agent_id`. Detached on the executor like
/// the run pane's reader.
pub fn spawn_term_reader(
    executor: &Executor,
    session: Arc<dyn TerminalSession>,
    workspace: WorkspaceId,
    agent_id: TermId,
    pty_tx: mpsc::Sender<PtyNotification>,
) {
    executor
        .spawn(term_reader_task(session, workspace, agent_id, pty_tx))
        .detach();
}

async fn term_reader_task(
    session: Arc<dyn TerminalSession>,
    workspace: WorkspaceId,
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
                workspace,
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
        .send(PtyNotification::TermExited {
            workspace,
            term_id: agent_id,
        })
        .await;
}

/// Spawn the reader that pumps a remote `:ssh` session's PTY output toward
/// stdout. Detached on the executor like the terminal pane's reader.
pub fn spawn_ssh_reader(
    executor: &Executor,
    session: Arc<dyn TerminalSession>,
    pty_tx: mpsc::Sender<PtyNotification>,
) {
    executor.spawn(ssh_reader_task(session, pty_tx)).detach();
}

async fn ssh_reader_task(session: Arc<dyn TerminalSession>, tx: mpsc::Sender<PtyNotification>) {
    loop {
        let chunk = match session.read_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        if tx
            .send(PtyNotification::SshOutput { data: chunk })
            .await
            .is_err()
        {
            break;
        }
    }

    // The read loop ends when ssh closes its PTY end, which is the remote
    // editor exiting. The code decides whether the status bar reports a
    // failure, so it is read after the loop rather than guessed from it.
    let exit_status = session.try_wait().await.ok().flatten();
    let _ = tx.send(PtyNotification::SshExited { exit_status }).await;
}

/// Filesystem path of the per-session agent hook socket for `uid`, in
/// [`agent_socket_dir`].
///
/// Passed as `STOAT_AGENT_SOCK` to both kinds of shell this instance owns, the
/// Claude subshell and a terminal pane's own shell. The in-process IPC server
/// binds the same path, so a hook callback from the one and a `stoat <file>`
/// from the other both reach the owning session.
pub fn agent_socket_path(uid: WorkspaceUid) -> std::io::Result<PathBuf> {
    Ok(agent_socket_path_in(&agent_socket_dir()?, uid))
}

/// Filesystem path of the per-session agent hook socket for `uid` under `dir`.
///
/// The naming half of [`agent_socket_path`], split out so a caller holding a
/// directory of its own resolves the same name without touching the real
/// environment. [`crate::Stoat::set_agent_socket_dir`] is that caller.
pub fn agent_socket_path_in(dir: &Path, uid: WorkspaceUid) -> PathBuf {
    dir.join(agent_socket_name(uid))
}

/// The directory that holds the per-session agent sockets, as
/// [`stoat_log::socket_dir`] picks it with the Stoat state directory first.
///
/// Every uid prints as 16 hex digits, so all session sockets have one name
/// length and the pick is the same for each of them. Every process that binds
/// a socket or reaches one by its session uid resolves the directory here, so
/// they agree on the path.
///
/// See also:
/// - [`agent_socket_bind_dir`] to make the directory ready for a bind.
pub fn agent_socket_dir() -> std::io::Result<PathBuf> {
    Ok(stoat_log::socket_dir(
        stoat_log::state_dir()?,
        &agent_socket_name(WorkspaceUid(0)),
    ))
}

/// [`agent_socket_dir`], ready for this process to bind sockets in, as
/// [`stoat_log::socket_bind_dir`] prepares it.
pub fn agent_socket_bind_dir() -> std::io::Result<PathBuf> {
    stoat_log::socket_bind_dir(stoat_log::state_dir()?, &agent_socket_name(WorkspaceUid(0)))
}

/// The file name of the agent hook socket for `uid`.
fn agent_socket_name(uid: WorkspaceUid) -> String {
    format!("agent-{uid}.sock")
}

/// The spawn arguments for an owned Claude subshell.
///
/// `EDITOR` and `VISUAL` push whether or not `socket_path` is given: the
/// agent's blocking-editor contract is how it composes a prompt, and does not
/// depend on this instance having a socket directory. The session pair does
/// depend on one, so it pushes only alongside a path.
fn claude_spawn_args(
    uid: WorkspaceUid,
    cwd: &Path,
    socket_path: Option<&Path>,
    editor_command: &str,
    diff: &[(String, Option<String>)],
) -> SpawnArgs {
    let mut args = SpawnArgs {
        program: "claude".into(),
        args: Vec::new(),
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        width: 80,
        rows: 24,
    };
    merge_env_diff(&mut args, diff);
    if let Some(socket_path) = socket_path {
        args.env.extend([
            ("STOAT_SESSION".into(), uid.to_string()),
            (
                "STOAT_AGENT_SOCK".into(),
                socket_path.to_string_lossy().into_owned(),
            ),
        ]);
    }
    args.env.extend([
        ("EDITOR".into(), editor_command.to_string()),
        ("VISUAL".into(), editor_command.to_string()),
    ]);
    args
}

/// The `$EDITOR` command the owned agent runs to compose prompts in the IDE.
///
/// Resolves the current executable so the agent invokes this same binary's
/// `api open --wait --split` even when a bare `stoat` is not on its PATH. The
/// command reads the socket from the already-injected `STOAT_AGENT_SOCK`, opens
/// the file in a split beside the focused pane, and holds until it closes, so
/// no further arguments are baked in.
fn editor_bridge_command() -> String {
    editor_command_for(std::env::current_exe().ok().as_deref())
}

/// Format the editor command from a resolved executable path.
///
/// Falls back to a bare `stoat api open --wait --split` when the path is
/// unknown, relying on PATH resolution in that case.
fn editor_command_for(exe: Option<&Path>) -> String {
    match exe {
        Some(exe) => format!("{} api open --wait --split", exe.to_string_lossy()),
        None => "stoat api open --wait --split".to_string(),
    }
}

/// Forward each chunk that `session` reads to `sink`, then the exit status at
/// the PTY's end.
///
/// The PTY closes before the kernel lets the child be reaped, so the status
/// comes from `try_wait`, asked up to [`EXIT_STATUS_TRIES`] times,
/// [`EXIT_STATUS_POLL`] apart. A status that never comes reads `None`.
async fn cmd_reader_task(session: Arc<dyn TerminalSession>, sink: CmdSink, executor: Executor) {
    let CmdSink {
        tx,
        workspace,
        cmd,
        run,
    } = sink;
    loop {
        let data = match session.read_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let output = PtyNotification::CmdOutput {
            workspace,
            cmd,
            run,
            data,
        };
        if tx.send(output).await.is_err() {
            return;
        }
    }

    let mut exit_status = None;
    for _ in 0..EXIT_STATUS_TRIES {
        if let Ok(Some(code)) = session.try_wait().await {
            exit_status = Some(code);
            break;
        }
        executor.timer(EXIT_STATUS_POLL).await;
    }
    let _ = tx
        .send(PtyNotification::CmdExited {
            workspace,
            cmd,
            run,
            exit_status,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeTerminalSession;
    use stoat_scheduler::TestScheduler;

    /// Without a socket directory there is no instance to name, but the agent
    /// still composes prompts through the editor bridge.
    #[test]
    fn claude_args_keep_the_editor_pair_without_a_socket() {
        let args = claude_spawn_args(
            WorkspaceUid(0xABCD),
            Path::new("/work"),
            None,
            "/usr/bin/stoat api open --wait --split",
            &[],
        );
        assert_eq!(
            args.env,
            vec![
                (
                    "EDITOR".to_string(),
                    "/usr/bin/stoat api open --wait --split".to_string()
                ),
                (
                    "VISUAL".to_string(),
                    "/usr/bin/stoat api open --wait --split".to_string()
                ),
            ],
        );
    }

    #[test]
    fn claude_args_inject_session_socket_and_editor_env() {
        let uid = WorkspaceUid(0xABCD);
        let diff = vec![
            ("FLAKE_VAR".to_string(), Some("1".to_string())),
            ("OLD_VAR".to_string(), None),
        ];
        let args = claude_spawn_args(
            uid,
            Path::new("/work"),
            Some(Path::new("/run/agent.sock")),
            "/usr/bin/stoat api open --wait --split",
            &diff,
        );
        assert_eq!(args.program, "claude");
        assert_eq!(args.cwd, Path::new("/work"));
        assert_eq!(args.env_remove, vec!["OLD_VAR".to_string()]);
        assert_eq!(
            args.env,
            vec![
                // The diff's set lands first, so the built-ins below win any
                // key conflict under open_local_pty's last-writer semantics.
                ("FLAKE_VAR".to_string(), "1".to_string()),
                ("STOAT_SESSION".to_string(), uid.to_string()),
                (
                    "STOAT_AGENT_SOCK".to_string(),
                    "/run/agent.sock".to_string()
                ),
                (
                    "EDITOR".to_string(),
                    "/usr/bin/stoat api open --wait --split".to_string()
                ),
                (
                    "VISUAL".to_string(),
                    "/usr/bin/stoat api open --wait --split".to_string()
                ),
            ],
        );
    }

    #[test]
    fn terminal_args_inject_the_session_triple_only_when_asked() {
        let uid = WorkspaceUid(0xABCD);
        let diff = vec![
            ("FLAKE_VAR".to_string(), Some("1".to_string())),
            ("OLD_VAR".to_string(), None),
        ];
        let session_env = TermSpawnEnv {
            uid,
            socket_path: PathBuf::from("/run/agent.sock"),
            token: 7,
        };
        let args = terminal_spawn_args(
            Path::new("/work"),
            "/bin/zsh",
            &["-l".to_string()],
            &diff,
            Some(&session_env),
        );
        assert_eq!(args.program, "/bin/zsh");
        assert_eq!(args.args, vec!["-l".to_string()]);
        assert_eq!(args.cwd, Path::new("/work"));
        assert_eq!(args.env_remove, vec!["OLD_VAR".to_string()]);
        assert_eq!(
            args.env,
            vec![
                // The diff's set lands first, so the built-ins below win any
                // key conflict under open_local_pty's last-writer semantics.
                ("FLAKE_VAR".to_string(), "1".to_string()),
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("STOAT_SESSION".to_string(), uid.to_string()),
                (
                    "STOAT_AGENT_SOCK".to_string(),
                    "/run/agent.sock".to_string()
                ),
                ("STOAT_TERM_ID".to_string(), "7".to_string()),
            ],
        );

        let bare = terminal_spawn_args(Path::new("/work"), "/bin/zsh", &[], &diff, None);
        assert_eq!(
            bare.env,
            vec![
                ("FLAKE_VAR".to_string(), "1".to_string()),
                ("TERM".to_string(), "xterm-256color".to_string()),
            ],
            "without a session env the shell learns only its terminal type",
        );
    }

    #[test]
    fn agent_socket_named_under_the_given_dir() {
        assert_eq!(
            agent_socket_path_in(Path::new("/state"), WorkspaceUid(0xABCD)),
            Path::new("/state/agent-000000000000abcd.sock"),
        );
    }

    #[test]
    fn editor_command_uses_exe_path_with_fallback() {
        assert_eq!(
            editor_command_for(Some(Path::new("/usr/bin/stoat"))),
            "/usr/bin/stoat api open --wait --split"
        );
        assert_eq!(editor_command_for(None), "stoat api open --wait --split");
    }

    /// The reader forwards the chunk and then, at the PTY's end, the exit
    /// status, both named by the sink's Cmd and run.
    #[test]
    fn the_reader_reports_the_exit_status_after_the_last_chunk() {
        let scheduler = Arc::new(TestScheduler::new());
        let session = Arc::new(FakeTerminalSession::new());
        session.push_output(b"hello");
        session.finish(7);
        let (tx, mut rx) = mpsc::channel(8);
        let sink = CmdSink {
            tx,
            workspace: WorkspaceId::default(),
            cmd: CmdId::default(),
            run: 3,
        };

        scheduler.block_on(cmd_reader_task(session, sink, scheduler.executor()));

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(match event {
                PtyNotification::CmdOutput { cmd, run, data, .. } => {
                    format!(
                        "output {} {run} {}",
                        cmd == CmdId::default(),
                        String::from_utf8_lossy(&data)
                    )
                },
                PtyNotification::CmdExited {
                    cmd,
                    run,
                    exit_status,
                    ..
                } => format!("exited {} {run} {exit_status:?}", cmd == CmdId::default()),
                _ => "other".to_owned(),
            });
        }
        assert_eq!(events, ["output true 3 hello", "exited true 3 Some(7)"]);
    }
}
