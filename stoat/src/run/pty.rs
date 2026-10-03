use super::RunId;
use crate::{
    host::terminal::{merge_env_diff, open_local_pty, SpawnArgs, TerminalHost, TerminalSession},
    term_session::TermId,
    workspace::WorkspaceUid,
};
use std::{
    fs::{self, DirBuilder},
    io::{Error, ErrorKind},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::SocketAddr,
    },
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
            PtyNotification::Output { data, .. }
            | PtyNotification::TermOutput { data, .. }
            | PtyNotification::SshOutput { data } => data.len(),
            PtyNotification::CommandDone { .. }
            | PtyNotification::TermExited { .. }
            | PtyNotification::SshExited { .. } => 0,
        }
    }
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
    diff: &[(String, Option<String>)],
) -> std::io::Result<ShellHandle> {
    use futures::FutureExt;

    let mut args = SpawnArgs {
        program: "bash".into(),
        args: vec!["--noediting".into(), "--noprofile".into(), "--norc".into()],
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    merge_env_diff(&mut args, diff);
    args.env.extend([
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
    ]);
    let session = match host.spawn(args).now_or_never() {
        Some(result) => result?,
        None => {
            return Err(Error::other(
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
    diff: &[(String, Option<String>)],
) -> std::io::Result<ShellHandle> {
    let mut args = SpawnArgs {
        program: "bash".into(),
        args: vec!["-c".into(), command.to_string()],
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        width,
        rows: 24,
    };
    merge_env_diff(&mut args, diff);
    args.env.push(("TERM".into(), "dumb".into()));
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
    dir.join(format!("agent-{uid}.sock"))
}

/// Directory holding the per-session agent sockets.
///
/// It is the Stoat state directory when a socket path there fits in a Unix
/// socket address. That address holds 104 bytes on macOS and 108 on Linux.
/// Otherwise it is a directory of this user's own in the runtime directory:
/// `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` elsewhere, and `/tmp` without either.
///
/// Every process that binds a socket or reaches one by its session uid
/// resolves the directory here, so they agree on the path. Creates nothing.
///
/// See also:
/// - [`agent_socket_bind_dir`] to make the directory ready for a bind.
pub fn agent_socket_dir() -> std::io::Result<PathBuf> {
    Ok(agent_socket_dir_in(
        stoat_log::state_dir()?,
        runtime_dir(),
        user_id(),
    ))
}

/// [`agent_socket_dir`], ready for this process to bind sockets in.
///
/// A directory in the runtime directory is created private to this user. An
/// existing one is refused when another user owns it or other users have
/// access to it. Any user with access to the directory controls the sockets in
/// it. The state directory is used as found.
pub fn agent_socket_bind_dir() -> std::io::Result<PathBuf> {
    let state = stoat_log::state_dir()?;
    let user = user_id();
    let dir = agent_socket_dir_in(state.clone(), runtime_dir(), user);
    if dir != state {
        ensure_private_dir(&dir, user)?;
    }
    Ok(dir)
}

/// The directory [`agent_socket_dir`] picks from the state directory, the
/// runtime directory, and the user id that names the fallback.
fn agent_socket_dir_in(state: PathBuf, runtime: Option<PathBuf>, user: u32) -> PathBuf {
    // Every uid prints as 16 hex digits, so one socket path's length is the
    // length of all of them.
    if SocketAddr::from_pathname(agent_socket_path_in(&state, WorkspaceUid(0))).is_ok() {
        return state;
    }
    runtime
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("stoat-{user}"))
}

/// The runtime directory the environment names, `$TMPDIR` on macOS and
/// `$XDG_RUNTIME_DIR` elsewhere.
// The env read is the blessed boundary. It hands its value straight to
// agent_socket_dir_in, which is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
fn runtime_dir() -> Option<PathBuf> {
    let var = if cfg!(target_os = "macos") {
        "TMPDIR"
    } else {
        "XDG_RUNTIME_DIR"
    };
    std::env::var_os(var).map(PathBuf::from)
}

/// The real user id of this process.
fn user_id() -> u32 {
    // SAFETY: `getuid` takes no arguments and always succeeds.
    unsafe { libc::getuid() }
}

/// Create `dir` private to `user`, or make sure that it is private already.
///
/// An existing directory passes when `user` owns it and no other user has
/// access to it. Anything else is a [`ErrorKind::PermissionDenied`] error.
// The directory holds the sockets the listeners bind. That is socket lifecycle,
// not the user-file IO that FsHost abstracts, and no fake host binds a socket.
fn ensure_private_dir(dir: &Path, user: u32) -> std::io::Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {},
        created => return created,
    }
    let meta = fs::symlink_metadata(dir)?;
    if meta.is_dir() && meta.uid() == user && meta.mode() & 0o077 == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!("{} is not private to this user", dir.display()),
    ))
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
    use std::os::unix::fs::{self as unix_fs, PermissionsExt};

    /// Without a socket directory there is no instance to name, but the agent
    /// still composes prompts through the editor bridge.
    #[test]
    fn claude_args_keep_the_editor_pair_without_a_socket() {
        let args = claude_spawn_args(
            WorkspaceUid(0xABCD),
            Path::new("/work"),
            None,
            "/usr/bin/stoat editor",
            &[],
        );
        assert_eq!(
            args.env,
            vec![
                ("EDITOR".to_string(), "/usr/bin/stoat editor".to_string()),
                ("VISUAL".to_string(), "/usr/bin/stoat editor".to_string()),
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
            "/usr/bin/stoat editor",
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
                ("EDITOR".to_string(), "/usr/bin/stoat editor".to_string()),
                ("VISUAL".to_string(), "/usr/bin/stoat editor".to_string()),
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

    /// A socket path the state directory has no room for moves to a directory
    /// of this user's own in the runtime directory, or in `/tmp` without one.
    #[test]
    fn a_long_state_dir_moves_the_sockets_to_the_runtime_dir() {
        let long = PathBuf::from(format!("/{}", "s".repeat(100)));
        let runtime = Some(PathBuf::from("/run/user/7"));
        assert_eq!(
            [
                agent_socket_dir_in(PathBuf::from("/state"), runtime.clone(), 7),
                agent_socket_dir_in(long.clone(), runtime, 7),
                agent_socket_dir_in(long, None, 7),
            ],
            [
                PathBuf::from("/state"),
                PathBuf::from("/run/user/7/stoat-7"),
                PathBuf::from("/tmp/stoat-7"),
            ],
        );
    }

    /// Other users share the runtime directory, so a socket directory there is
    /// one this user creates private or finds private.
    #[test]
    fn a_runtime_socket_dir_is_private_to_its_user() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().metadata().unwrap().uid();
        let created = root.path().join("created");
        let open = root.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");

        let first = ensure_private_dir(&created, user);
        unix_fs::symlink(&created, &link).unwrap();
        let kinds = [
            first,
            ensure_private_dir(&created, user),
            ensure_private_dir(&created, user + 1),
            ensure_private_dir(&open, user),
            ensure_private_dir(&link, user),
        ]
        .map(|result| result.map_err(|err| err.kind()));
        let mode = created.metadata().unwrap().mode() & 0o777;

        assert_eq!(
            (kinds, mode),
            (
                [
                    Ok(()),
                    Ok(()),
                    Err(ErrorKind::PermissionDenied),
                    Err(ErrorKind::PermissionDenied),
                    Err(ErrorKind::PermissionDenied),
                ],
                0o700,
            ),
            "created, its own, another owner, open to others, a planted link",
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
