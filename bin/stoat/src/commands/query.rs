use clap::{Args, Subcommand};
use serde::Serialize;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use stoat::{
    host::{FsHost, LocalFs},
    run,
    workspace::WorkspaceUid,
};

/// Subcommands that interrogate a live session over its per-session socket and
/// print the JSON reply.
#[derive(Subcommand, Debug)]
pub enum QueryCommand {
    /// LSP host liveness, and each server's process id and capabilities.
    LspStatus {
        #[command(flatten)]
        socket: SocketArgs,
    },
    /// Diagnostics for `--path`, or every tracked path when it is omitted.
    Diagnostics {
        /// File to report diagnostics for. Omit for all paths.
        #[arg(long)]
        path: Option<PathBuf>,
        #[command(flatten)]
        socket: SocketArgs,
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
        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Flags shared by every query that select which session socket to reach.
#[derive(Args, Debug)]
pub struct SocketArgs {
    /// Target session uid (hex, as in STOAT_SESSION). Resolves to its
    /// per-session socket.
    #[arg(long)]
    session: Option<String>,

    /// Explicit socket path. Overrides `--session` and auto-detection.
    #[arg(long)]
    socket: Option<PathBuf>,
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

pub fn run(sub: QueryCommand) -> Result<(), Whatever> {
    let (request, socket_args) = match sub {
        QueryCommand::LspStatus { socket } => (QueryRequest::LspStatus, socket),
        QueryCommand::Diagnostics { path, socket } => (QueryRequest::Diagnostics { path }, socket),
        QueryCommand::Hover {
            path,
            line,
            col,
            socket,
        } => (QueryRequest::Hover { path, line, col }, socket),
    };

    let socket_path = resolve_socket_path(&socket_args)?;
    let reply = query(&socket_path, &request)?;
    print!("{reply}");
    Ok(())
}

/// Resolve which session socket to query.
///
/// `--socket` wins as an explicit path. Otherwise a `--session` uid maps to its
/// [`run::agent_socket_path`]. With neither, [`run::agent_socket_dir`] is
/// scanned for a sole live `agent-*.sock`.
fn resolve_socket_path(args: &SocketArgs) -> Result<PathBuf, Whatever> {
    if let Some(socket) = &args.socket {
        return Ok(socket.clone());
    }
    if let Some(session) = &args.session {
        let uid = parse_session_uid(session)?;
        return run::agent_socket_path(uid).whatever_context("resolve agent socket path");
    }
    find_sole_socket()
}

/// Locate the one live session socket in [`run::agent_socket_dir`] when neither
/// `--socket` nor `--session` pins it down.
fn find_sole_socket() -> Result<PathBuf, Whatever> {
    let dir = run::agent_socket_dir().whatever_context("resolve agent socket directory")?;
    sole_live_socket(&dir, &LocalFs)
}

/// The one live `agent-*.sock` in `dir`.
///
/// A session that ends normally removes its socket, but a session that SIGKILL
/// or a crash ends leaves it behind. A candidate that refuses a connection has no session
/// behind it, so this removes it through `fs` and skips it. Any other connect
/// failure skips the candidate and keeps the file, since nothing proves it dead.
///
/// Zero or several live sockets is an error that names the candidates. The
/// caller then picks one with `--session` or `--socket`.
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
        whatever!("multiple session sockets, pass --session or --socket: {sockets:?}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn wire(request: &QueryRequest) -> String {
        serde_json::to_string(request).unwrap()
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
    fn explicit_socket_overrides_session() {
        let args = SocketArgs {
            session: Some("abc".into()),
            socket: Some(PathBuf::from("/run/session.sock")),
        };
        assert_eq!(
            resolve_socket_path(&args).unwrap(),
            PathBuf::from("/run/session.sock")
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
    fn two_live_sockets_are_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let sockets = [
            dir.path().join("agent-1.sock"),
            dir.path().join("agent-2.sock"),
        ];
        let _listeners = sockets
            .each_ref()
            .map(|path| UnixListener::bind(path).unwrap());

        assert_eq!(
            sole_live_socket(dir.path(), &LocalFs)
                .unwrap_err()
                .to_string(),
            format!("multiple session sockets, pass --session or --socket: {sockets:?}"),
        );
    }
}
