use clap::{Args, Subcommand};
use serde::Serialize;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use stoat::{log, run, workspace::WorkspaceUid};

/// Subcommands that interrogate a live session over its per-session socket and
/// print the JSON reply.
#[derive(Subcommand, Debug)]
pub enum QueryCommand {
    /// LSP host liveness and the server's capabilities.
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
/// [`run::agent_socket_path`]. With neither, the state directory is scanned for a
/// sole `agent-*.sock`.
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

/// Locate the one live session socket when neither `--socket` nor `--session`
/// pins it down.
///
/// Scans [`log::state_dir`] for `agent-*.sock`. A single match is used. Zero or
/// several is an error naming the candidates, so the caller can disambiguate.
fn find_sole_socket() -> Result<PathBuf, Whatever> {
    let dir = log::state_dir().whatever_context("resolve state directory")?;
    let entries = std::fs::read_dir(&dir).whatever_context(format!("scan {}", dir.display()))?;

    let mut sockets = Vec::new();
    for entry in entries {
        let path = entry.whatever_context("read directory entry")?.path();
        let is_socket = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agent-") && name.ends_with(".sock"));
        if is_socket {
            sockets.push(path);
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
}
