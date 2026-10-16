use clap::{Args, Subcommand};
use serde::Serialize;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use stoat::{
    agent_status::AgentHookEvent,
    host::{FsHost, LocalFs},
    run,
    workspace::WorkspaceUid,
};

/// A `stoat api` invocation, which names one verb and, optionally, the session
/// it reaches.
#[derive(Args, Debug)]
pub struct ApiArgs {
    /// The session to reach, by the hex uid that STOAT_SESSION carries.
    /// Without it, the environment names the session, or else the sole live
    /// session.
    #[arg(long, global = true, value_name = "ID")]
    session: Option<String>,

    #[command(subcommand)]
    sub: ApiCommand,
}

/// The verbs of `stoat api`, each sent to one session over its per-session
/// socket.
///
/// A query verb prints the session's one-line JSON reply. `hook` gets no reply
/// and prints nothing.
#[derive(Subcommand, Debug)]
pub enum ApiCommand {
    /// LSP host liveness, and each server's process id and capabilities.
    LspStatus,
    /// Diagnostics for `--path`, or every tracked path when it is omitted.
    Diagnostics {
        /// File to report diagnostics for. Omit for all paths.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Hover at an LSP position in a file open in the session.
    Hover {
        /// File to hover in. Must already be open in the session.
        #[arg(long)]
        path: PathBuf,
        /// Zero-based LSP (UTF-16) line.
        #[arg(long)]
        line: u32,
        /// Zero-based LSP (UTF-16) character column.
        #[arg(long)]
        col: u32,
    },
    /// Send one hook event to the owning session's IPC socket and exit. The
    /// owned Claude subshell's hooks run this to push status into the session.
    Hook {
        /// One of session-start, pre-tool-use, post-tool-use, notification,
        /// stop, or session-end.
        name: String,

        /// Extra values for the hook. pre-tool-use takes the tool name as its
        /// first value. Other hooks take none.
        values: Vec<String>,
    },
}

/// One query in its `req`-tagged wire form, matching the session server's
/// request decoder.
#[derive(Serialize, Debug)]
#[serde(tag = "req", rename_all = "kebab-case")]
enum QueryRequest {
    LspStatus,
    Diagnostics {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    Hover {
        path: PathBuf,
        line: u32,
        col: u32,
    },
}

// The env reads are the blessed boundary. They hand the values straight to
// resolve_socket, which is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
pub fn run(args: ApiArgs) -> Result<(), Whatever> {
    let socket_path = {
        let dir = run::agent_socket_dir().whatever_context("resolve agent socket directory")?;
        resolve_socket(
            args.session.as_deref(),
            std::env::var("STOAT_AGENT_SOCK").ok(),
            std::env::var("STOAT_SESSION").ok(),
            &dir,
            &LocalFs,
        )?
    };

    let request = match args.sub {
        ApiCommand::LspStatus => QueryRequest::LspStatus,
        ApiCommand::Diagnostics { path } => QueryRequest::Diagnostics { path },
        ApiCommand::Hover { path, line, col } => QueryRequest::Hover { path, line, col },
        ApiCommand::Hook { name, values } => {
            return send_event(&socket_path, &build_event(&name, &values)?);
        },
    };

    let reply = query(&socket_path, &request)?;
    print!("{reply}");
    Ok(())
}

/// Resolve which session socket to reach.
///
/// A `selector` uid wins, since a person who typed `--session` meant it. The
/// spawn-injected `env_sock` path comes next, so a hook from a child whose XDG
/// environment differs from its parent's still reaches the parent. An
/// `env_session` uid comes third. With none of them, it scans `dir` for the
/// sole live session.
fn resolve_socket(
    selector: Option<&str>,
    env_sock: Option<String>,
    env_session: Option<String>,
    dir: &Path,
    fs: &dyn FsHost,
) -> Result<PathBuf, Whatever> {
    if let Some(selector) = selector {
        return Ok(run::agent_socket_path_in(dir, parse_session_uid(selector)?));
    }
    if let Some(path) = env_sock {
        return Ok(PathBuf::from(path));
    }
    if let Some(uid) = env_session {
        return Ok(run::agent_socket_path_in(dir, parse_session_uid(&uid)?));
    }
    sole_live_socket(dir, fs)
}

/// The one live `agent-*.sock` in `dir`.
///
/// A session that ends normally removes its socket, but a session that SIGKILL
/// or a crash ends leaves it behind. A candidate that refuses a connection has no session
/// behind it, so this removes it through `fs` and skips it. Any other connect
/// failure skips the candidate and keeps the file, since nothing proves it dead.
///
/// Zero or several live sockets is an error that names the candidates. The
/// caller then picks one with `--session`.
fn sole_live_socket(dir: &Path, fs: &dyn FsHost) -> Result<PathBuf, Whatever> {
    let entries = fs
        .list_dir(dir)
        .whatever_context(format!("scan {}", dir.display()))?;

    let mut sockets = Vec::new();
    for entry in entries {
        if !(entry.name.starts_with("agent-") && entry.name.ends_with(".sock")) {
            continue;
        }
        let path = dir.join(entry.name.as_str());

        // A live session reads the probe as a connection that closes at once.
        match UnixStream::connect(&path) {
            Ok(_) => sockets.push(path),
            Err(err) if err.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs.remove_file(&path);
            },
            Err(_) => {},
        }
    }

    if sockets.is_empty() {
        whatever!("no session sockets found in {}", dir.display());
    }
    if sockets.len() > 1 {
        sockets.sort();
        let names: Vec<_> = sockets
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy())
            .collect();
        whatever!(
            "several sessions are live, pass --session <ID>: {}",
            names.join(", ")
        );
    }
    Ok(sockets.into_iter().next().expect("one socket"))
}

fn parse_session_uid(session: &str) -> Result<WorkspaceUid, Whatever> {
    let raw = u64::from_str_radix(session.trim_start_matches("0x"), 16)
        .whatever_context(format!("parse session uid {session:?}"))?;
    Ok(WorkspaceUid(raw))
}

/// Send one request line and return the session's one-line JSON reply.
fn query(socket_path: &Path, request: &QueryRequest) -> Result<String, Whatever> {
    let line = serde_json::to_string(request).whatever_context("serialize query request")?;
    let mut stream = UnixStream::connect(socket_path).whatever_context(format!(
        "connect to session socket {}",
        socket_path.display()
    ))?;
    stream
        .write_all(line.as_bytes())
        .whatever_context("write query request")?;
    stream
        .write_all(b"\n")
        .whatever_context("write request terminator")?;

    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .whatever_context("read query reply")?;
    if reply.is_empty() {
        whatever!("session closed without a reply");
    }
    Ok(reply)
}

/// Map a CLI hook name and its trailing values to the typed wire event.
///
/// `pre-tool-use` consumes the first value as the tool name. The other hooks
/// carry no fields. An unrecognized name or a `pre-tool-use` with no tool is a
/// usage error.
fn build_event(name: &str, values: &[String]) -> Result<AgentHookEvent, Whatever> {
    let event = match name {
        "session-start" => AgentHookEvent::SessionStart,
        "pre-tool-use" => match values.first() {
            Some(tool) => AgentHookEvent::PreToolUse { tool: tool.clone() },
            None => whatever!("pre-tool-use requires a tool name argument"),
        },
        "post-tool-use" => AgentHookEvent::PostToolUse,
        "notification" => AgentHookEvent::Notification,
        "stop" => AgentHookEvent::Stop,
        "session-end" => AgentHookEvent::SessionEnd,
        other => whatever!("unknown hook name: {other}"),
    };
    Ok(event)
}

fn send_event(socket_path: &Path, event: &AgentHookEvent) -> Result<(), Whatever> {
    let line = serde_json::to_string(event).whatever_context("serialize hook event")?;
    let mut stream =
        UnixStream::connect(socket_path).whatever_context("connect to agent socket")?;
    stream
        .write_all(line.as_bytes())
        .whatever_context("write hook event")?;
    stream
        .write_all(b"\n")
        .whatever_context("write hook event terminator")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn wire(request: &QueryRequest) -> String {
        serde_json::to_string(request).unwrap()
    }

    fn hook_wire(name: &str, values: &[&str]) -> String {
        let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        serde_json::to_string(&build_event(name, &values).unwrap()).unwrap()
    }

    /// Resolves against a directory nothing reads, since a named session needs
    /// no scan.
    fn resolved(
        selector: Option<&str>,
        env_sock: Option<&str>,
        env_session: Option<&str>,
    ) -> PathBuf {
        resolve_socket(
            selector,
            env_sock.map(String::from),
            env_session.map(String::from),
            Path::new("/state"),
            &LocalFs,
        )
        .unwrap()
    }

    #[test]
    fn requests_serialize_to_wire_form() {
        assert_eq!(wire(&QueryRequest::LspStatus), r#"{"req":"lsp-status"}"#);
        assert_eq!(
            wire(&QueryRequest::Diagnostics { path: None }),
            r#"{"req":"diagnostics"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Diagnostics {
                path: Some(PathBuf::from("/a.rs")),
            }),
            r#"{"req":"diagnostics","path":"/a.rs"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Hover {
                path: PathBuf::from("/a.rs"),
                line: 3,
                col: 5,
            }),
            r#"{"req":"hover","path":"/a.rs","line":3,"col":5}"#
        );
    }

    #[test]
    fn the_session_flag_wins_over_the_environment() {
        assert_eq!(
            resolved(
                Some("0000000000000abc"),
                Some("/run/agent.sock"),
                Some("0000000000000def"),
            ),
            PathBuf::from("/state/agent-0000000000000abc.sock"),
        );
    }

    #[test]
    fn the_spawn_injected_path_wins_over_the_environment_uid() {
        assert_eq!(
            resolved(None, Some("/run/agent.sock"), Some("0000000000000def")),
            PathBuf::from("/run/agent.sock"),
        );
    }

    #[test]
    fn a_session_uid_from_the_environment_needs_no_flag() {
        let uid = WorkspaceUid(0xDEF).to_string();
        assert_eq!(
            resolved(None, None, Some(&uid)),
            PathBuf::from("/state/agent-0000000000000def.sock"),
        );
    }

    #[test]
    fn session_uid_parses_hex() {
        assert_eq!(parse_session_uid("abcd").unwrap(), WorkspaceUid(0xABCD));
        assert_eq!(parse_session_uid("0xabcd").unwrap(), WorkspaceUid(0xABCD));
        assert!(parse_session_uid("nothex").is_err());
    }

    #[test]
    fn a_dead_socket_beside_the_live_one_goes() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("agent-1.sock");
        let dead = dir.path().join("agent-2.sock");
        let _listener = UnixListener::bind(&live).unwrap();
        drop(UnixListener::bind(&dead).unwrap());

        assert_eq!(sole_live_socket(dir.path(), &LocalFs).unwrap(), live);
        assert!(!dead.exists(), "the dead socket stays in the state dir");
    }

    #[test]
    fn with_nothing_named_and_two_live_sockets_the_error_names_both() {
        let dir = tempfile::tempdir().unwrap();
        let _listeners = ["agent-1.sock", "agent-2.sock"]
            .map(|name| UnixListener::bind(dir.path().join(name)).unwrap());

        assert_eq!(
            resolve_socket(None, None, None, dir.path(), &LocalFs)
                .unwrap_err()
                .to_string(),
            "several sessions are live, pass --session <ID>: agent-1.sock, agent-2.sock",
        );
    }

    #[test]
    fn builds_each_hook_to_wire_form() {
        assert_eq!(
            hook_wire("session-start", &[]),
            r#"{"hook":"session-start"}"#
        );
        assert_eq!(
            hook_wire("pre-tool-use", &["Bash"]),
            r#"{"hook":"pre-tool-use","tool":"Bash"}"#
        );
        assert_eq!(
            hook_wire("post-tool-use", &[]),
            r#"{"hook":"post-tool-use"}"#
        );
        assert_eq!(hook_wire("notification", &[]), r#"{"hook":"notification"}"#);
        assert_eq!(hook_wire("stop", &[]), r#"{"hook":"stop"}"#);
        assert_eq!(hook_wire("session-end", &[]), r#"{"hook":"session-end"}"#);
    }

    #[test]
    fn unknown_hook_and_missing_tool_are_errors() {
        assert!(build_event("bogus", &[]).is_err());
        assert!(build_event("pre-tool-use", &[]).is_err());
    }
}
