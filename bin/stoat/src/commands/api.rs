use crate::commands::term_open::{self, Sent};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use snafu::{whatever, ResultExt, Whatever};
use std::{
    cmp::Reverse,
    io::{self, BufRead, BufReader, ErrorKind, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};
use stoat::{
    agent_status::AgentHookEvent,
    host::{FsHost, LocalFs},
    input_parse::{self, InputParseError},
    run,
    walkthrough::{self, Point, WalkthroughError},
    workspace::WorkspaceUid,
};

/// How long discovery waits on one session's `session-info` reply.
///
/// A healthy event loop answers within a frame. A stopped or wedged session
/// costs this much on each discovery, and discovery then leaves it out.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// A `stoat api` invocation, which names one verb and, optionally, the session
/// it reaches.
#[derive(Args, Debug)]
pub struct ApiArgs {
    /// The session to reach, by the hex uid that STOAT_SESSION carries or by
    /// its workspace name. Without it, the environment names the session, or
    /// else the live session whose root holds the working directory.
    #[arg(long, global = true, value_name = "ID")]
    session: Option<String>,

    #[command(subcommand)]
    sub: ApiCommand,
}

/// The verbs of `stoat api`.
///
/// `sessions` asks every live session. Every other verb reaches one session
/// over its per-session socket. A query verb prints the session's one-line
/// JSON reply. `watch` prints a line for each change until the session ends.
/// `hook` and `open` print nothing.
#[derive(Subcommand, Debug)]
pub enum ApiCommand {
    /// Every live session's `session-info` reply, most recently active first.
    Sessions,
    /// Open files in the session, beside the user.
    ///
    /// Inside a terminal pane the files cover that pane. Elsewhere they open in
    /// a fresh split with `--split`, and without it in the focused pane unless
    /// that pane shows the agent, a shell, or a run, which keep their panes.
    Open {
        /// Files to open. A relative path resolves against the working
        /// directory.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Hold until every opened file leaves the editor. Exit status 1
        /// reports a file left with unsaved edits, the `$EDITOR` contract.
        #[arg(long)]
        wait: bool,
        /// Open in a fresh split beside the focused pane when no terminal pane
        /// is the caller.
        #[arg(long)]
        split: bool,
    },
    /// Show a file in the session with a range selected and on screen, and
    /// print the range selected.
    ///
    /// The file shows in the focused pane unless that pane shows the agent, a
    /// shell, or a run, and then in another pane or a fresh split. Keyboard
    /// focus stays where it is.
    Goto {
        /// File to show. A relative path resolves against the working
        /// directory.
        path: PathBuf,
        /// The range as `L`, `L-L`, or `L:C-L:C`, with 1-based lines and byte
        /// columns and inclusive ends, the form the other verbs report.
        #[arg(value_parser = range_text)]
        range: String,
    },
    /// Run one action in the session, written as a key binding names it, such
    /// as `MoveDown` or `SetMode("insert")`, and print the action's name.
    ///
    /// The action runs as a key bound to the same text runs it. An action the
    /// editor does not know, or an argument that does not fit, prints an
    /// error.
    Act {
        /// The action, with its arguments in parentheses when it takes any.
        expr: String,
    },
    /// Type a key sequence into the session as a person types it, and print
    /// the count of keys sent.
    ///
    /// The keys go to what has keyboard focus, and that includes the agent's
    /// own terminal pane. A `<Wait:N>` step sends nothing.
    Keys {
        /// The keys in the `--inputs` grammar, such as `ihello<Esc>`, with
        /// named and modified keys in angle brackets.
        #[arg(value_parser = key_text)]
        keys: String,
    },
    /// The focused workspace, tab, pane, and modal, and the focused editor's
    /// file, cursor, and lines on screen.
    Focus {
        /// Also print the text of the lines on screen.
        #[arg(long)]
        text: bool,
    },
    /// Every selection of the focused editor, as 1-based inclusive points and a
    /// range in the walkthrough grammar.
    Selection {
        /// Also print the bytes each selection covers.
        #[arg(long)]
        text: bool,
    },
    /// Print the `focus` and `selection` reports, then print them again each
    /// time either one changes, until the session ends.
    ///
    /// Each line holds both reports, under `focus` and `selection`, without
    /// the text.
    Watch,
    /// Every buffer of the session's workspace, with its unsaved state and the
    /// tabs, panes, and docks that show it.
    Buffers,
    /// The terminal size, every tab with its panes, each pane's kind, title,
    /// file, and screen area, and every dock.
    Layout,
    /// LSP host liveness, and each server's process id and capabilities.
    LspStatus,
    /// Diagnostics for `--path`, or every tracked path when it is omitted.
    Diagnostics {
        /// File to report diagnostics for. Omit for all paths.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Hover at a position in a file open in the session.
    Hover {
        /// File to hover in. Must already be open in the session. A relative
        /// path resolves against the working directory.
        path: PathBuf,
        /// The position as `L:C`, a 1-based line and byte column, the form the
        /// other verbs report.
        #[arg(value_parser = walkthrough::parse_point)]
        position: Point,
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
    SessionInfo,
    Focus {
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        text: bool,
    },
    Selection {
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        text: bool,
    },
    Buffers,
    Layout,
    Diagnostics {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    Hover {
        path: PathBuf,
        line: u32,
        col: u32,
    },
    Goto {
        path: PathBuf,
        range: String,
    },
    Act {
        action: String,
    },
    Keys {
        keys: String,
    },
    Watch,
}

/// One live session's answer to `session-info`.
///
/// The pick reads the typed fields. The rest of the reply rides along in
/// `rest`, so `stoat api sessions` prints each reply whole.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SessionInfo {
    uid: String,
    name: String,
    git_root: PathBuf,
    active: bool,
    last_active_ms: u64,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

// The env and working-directory reads are the blessed boundary. They hand the
// values straight to resolve_socket, whose pick is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
pub fn run(args: ApiArgs) -> Result<(), Whatever> {
    let dir = run::agent_socket_dir().whatever_context("resolve agent socket directory")?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let socket = || {
        resolve_socket(
            args.session.as_deref(),
            std::env::var("STOAT_AGENT_SOCK").ok(),
            std::env::var("STOAT_SESSION").ok(),
            &cwd,
            &dir,
            &LocalFs,
        )
    };

    let request = match args.sub {
        ApiCommand::Sessions => {
            let sessions = live_sessions(&dir, &LocalFs)?;
            let replies: Vec<&SessionInfo> = sessions.iter().map(|(_, info)| info).collect();
            let line = serde_json::to_string(&replies).whatever_context("serialize sessions")?;
            println!("{line}");
            return Ok(());
        },
        ApiCommand::Focus { text } => QueryRequest::Focus { text },
        ApiCommand::Selection { text } => QueryRequest::Selection { text },
        ApiCommand::Buffers => QueryRequest::Buffers,
        ApiCommand::Layout => QueryRequest::Layout,
        ApiCommand::LspStatus => QueryRequest::LspStatus,
        ApiCommand::Diagnostics { path } => QueryRequest::Diagnostics { path },
        ApiCommand::Hover { path, position } => QueryRequest::Hover {
            path: cwd.join(path),
            line: position.line,
            col: position.col,
        },
        ApiCommand::Goto { path, range } => QueryRequest::Goto {
            path: cwd.join(path),
            range,
        },
        ApiCommand::Act { expr } => QueryRequest::Act { action: expr },
        ApiCommand::Keys { keys } => QueryRequest::Keys { keys },
        ApiCommand::Watch => return watch(&socket()?),
        ApiCommand::Hook { name, values } => {
            let event = build_event(&name, &values)?;
            return send_event(&socket()?, &event);
        },
        ApiCommand::Open { paths, wait, split } => {
            let term = std::env::var("STOAT_TERM_ID")
                .ok()
                .and_then(|id| id.parse::<u64>().ok());
            let paths: Vec<PathBuf> = paths.iter().map(|path| cwd.join(path)).collect();
            return open(&socket()?, &paths, term, wait, split);
        },
    };

    let reply = query(&socket()?, &request, None)?;
    print!("{reply}");
    Ok(())
}

/// Check `text` as a range with [`walkthrough::parse_range`], and keep the
/// text, which the session parses again.
fn range_text(text: &str) -> Result<String, WalkthroughError> {
    walkthrough::parse_range(text).map(|_| text.to_owned())
}

/// Check `text` as a key sequence with [`input_parse::parse_input_sequence`],
/// and keep the text, which the session parses again.
fn key_text(text: &str) -> Result<String, InputParseError> {
    input_parse::parse_input_sequence(text).map(|_| text.to_owned())
}

/// Resolve which session socket to reach.
///
/// A `selector` that parses as hex is a uid, and it wins, since a person who
/// typed `--session` meant it. The spawn-injected `env_sock` path comes next,
/// so a hook from a child whose XDG environment differs from its parent's
/// still reaches the parent. An `env_session` uid comes third. Otherwise it
/// probes the live sessions in `dir` and picks one by the `selector` name or by
/// `cwd`.
fn resolve_socket(
    selector: Option<&str>,
    env_sock: Option<String>,
    env_session: Option<String>,
    cwd: &Path,
    dir: &Path,
    fs: &dyn FsHost,
) -> Result<PathBuf, Whatever> {
    if let Some(selector) = selector {
        if let Ok(uid) = parse_session_uid(selector) {
            return Ok(run::agent_socket_path_in(dir, uid));
        }
    } else if let Some(path) = env_sock {
        return Ok(PathBuf::from(path));
    } else if let Some(uid) = env_session {
        return Ok(run::agent_socket_path_in(dir, parse_session_uid(&uid)?));
    }

    let sessions = live_sessions(dir, fs)?;
    // Session roots are absolute physical paths, so a relative or symlinked
    // `cwd` must resolve before the prefix match.
    let cwd = fs.canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    pick_session(&sessions, selector, &cwd).cloned()
}

/// Every live session in `dir` with its answer to `session-info`, most
/// recently active first.
///
/// A session that does not answer within [`PROBE_TIMEOUT`], such as one that
/// an older binary runs, is left out.
fn live_sessions(dir: &Path, fs: &dyn FsHost) -> Result<Vec<(PathBuf, SessionInfo)>, Whatever> {
    let mut sessions: Vec<(PathBuf, SessionInfo)> = live_sockets(dir, fs)?
        .into_iter()
        .filter_map(|socket| {
            let reply = query(&socket, &QueryRequest::SessionInfo, Some(PROBE_TIMEOUT)).ok()?;
            let info = serde_json::from_str(&reply).ok()?;
            Some((socket, info))
        })
        .collect();
    sessions.sort_by_key(|(_, info)| Reverse(info.last_active_ms));
    Ok(sessions)
}

/// The live `agent-*.sock` files in `dir`, sorted.
///
/// A session that ends normally removes its socket, but a session that SIGKILL
/// or a crash ends leaves it behind. A candidate that refuses a connection has no session
/// behind it, so this removes it through `fs` and skips it. Any other connect
/// failure skips the candidate and keeps the file, since nothing proves it dead.
fn live_sockets(dir: &Path, fs: &dyn FsHost) -> Result<Vec<PathBuf>, Whatever> {
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

    sockets.sort();
    Ok(sockets)
}

/// The socket of the session that `name`, or else `cwd`, picks.
///
/// A name matches a session's name case-insensitively. Without one, a session
/// matches when its git root holds `cwd`. Among matches the session on screen
/// wins, then the latest active one. With no match and no name, a sole session
/// is the pick. Otherwise the error lists every session, one per line.
fn pick_session<'a>(
    sessions: &'a [(PathBuf, SessionInfo)],
    name: Option<&str>,
    cwd: &Path,
) -> Result<&'a PathBuf, Whatever> {
    let wanted = name.map(str::to_lowercase);
    let best = sessions
        .iter()
        .filter(|(_, info)| match &wanted {
            Some(wanted) => info.name.to_lowercase() == *wanted,
            None => cwd.starts_with(&info.git_root),
        })
        .max_by_key(|(_, info)| (info.active, info.last_active_ms));
    if let Some((socket, _)) = best {
        return Ok(socket);
    }

    let listing = || -> String {
        sessions
            .iter()
            .map(|(_, info)| format!("\n{}  {}  {}", info.uid, info.name, info.git_root.display()))
            .collect()
    };
    match (name, sessions) {
        (_, []) => whatever!("no live session found"),
        (None, [(socket, _)]) => Ok(socket),
        (Some(name), _) => whatever!(
            "no live session is named {name:?}, pass --session <ID>:{}",
            listing()
        ),
        (None, _) => whatever!(
            "several sessions are live and none holds {}, pass --session <ID>:{}",
            cwd.display(),
            listing()
        ),
    }
}

fn parse_session_uid(session: &str) -> Result<WorkspaceUid, Whatever> {
    let raw = u64::from_str_radix(session.trim_start_matches("0x"), 16)
        .whatever_context(format!("parse session uid {session:?}"))?;
    Ok(WorkspaceUid(raw))
}

/// Send one request line and return the session's one-line JSON reply.
///
/// The write half closes after the request. A session that does not know the
/// request then ends the connection at once, where an open write half leaves
/// the read to wait on a reply that never comes. With `read_timeout`, a reply
/// slower than it is an error.
fn query(
    socket_path: &Path,
    request: &QueryRequest,
    read_timeout: Option<Duration>,
) -> Result<String, Whatever> {
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
    stream
        .shutdown(Shutdown::Write)
        .whatever_context("close the request half")?;
    stream
        .set_read_timeout(read_timeout)
        .whatever_context("set the reply timeout")?;

    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .whatever_context("read query reply")?;
    if reply.is_empty() {
        whatever!("session closed without a reply");
    }
    Ok(reply)
}

/// Send a `watch` request and print each line the session sends, until the
/// session ends the connection.
///
/// The write half stays open, because the session reads its end as the client
/// leaving. A reader that stops reading stdout, such as `head -n 1`, ends the
/// watch with success.
fn watch(socket_path: &Path) -> Result<(), Whatever> {
    let line =
        serde_json::to_string(&QueryRequest::Watch).whatever_context("serialize watch request")?;
    let mut stream = UnixStream::connect(socket_path).whatever_context(format!(
        "connect to session socket {}",
        socket_path.display()
    ))?;
    stream
        .write_all(line.as_bytes())
        .whatever_context("write watch request")?;
    stream
        .write_all(b"\n")
        .whatever_context("write request terminator")?;

    let mut out = io::stdout().lock();
    for view in BufReader::new(&stream).lines() {
        let view = view.whatever_context("read a view")?;
        if writeln!(out, "{view}").is_err() {
            return Ok(());
        }
    }
    Ok(())
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

/// Send an `open` request for `paths` and wait as the session answers.
///
/// A held open returns once its files close. A file left with unsaved edits
/// ends the process with status 1, the `$EDITOR` contract a waiting caller
/// relies on.
fn open(
    socket: &Path,
    paths: &[PathBuf],
    term: Option<u64>,
    wait: bool,
    split: bool,
) -> Result<(), Whatever> {
    let line = term_open::request_line(term, paths, wait, split);
    let sent = term_open::send(socket, &line).with_whatever_context(|error| {
        format!("reach the session at {}: {error}", socket.display())
    })?;
    match sent {
        Sent::Opened | Sent::Closed(0) => Ok(()),
        Sent::Closed(status) => std::process::exit(status),
        Sent::Unanswered => whatever!("the session closed without opening the files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{os::unix::net::UnixListener, thread};

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
            Path::new("/"),
            Path::new("/state"),
            &LocalFs,
        )
        .unwrap()
    }

    /// A session at `/state/agent-<uid>.sock` with the fields the pick reads.
    fn session(
        uid: &str,
        name: &str,
        root: &str,
        active: bool,
        last_active_ms: u64,
    ) -> (PathBuf, SessionInfo) {
        let info = SessionInfo {
            uid: uid.into(),
            name: name.into(),
            git_root: root.into(),
            active,
            last_active_ms,
            rest: Map::new(),
        };
        (PathBuf::from(format!("/state/agent-{uid}.sock")), info)
    }

    /// Serve `path` on a thread until a request line arrives, then answer it
    /// with `reply`, or close with no reply as a session of an older binary
    /// does.
    fn fake_session(path: &Path, reply: Option<Value>) {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                if request.is_empty() {
                    continue;
                }
                if let Some(reply) = &reply {
                    writeln!(stream, "{reply}").unwrap();
                }
                return;
            }
        });
    }

    #[test]
    fn requests_serialize_to_wire_form() {
        assert_eq!(wire(&QueryRequest::LspStatus), r#"{"req":"lsp-status"}"#);
        assert_eq!(
            wire(&QueryRequest::SessionInfo),
            r#"{"req":"session-info"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Focus { text: false }),
            r#"{"req":"focus"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Focus { text: true }),
            r#"{"req":"focus","text":true}"#
        );
        assert_eq!(
            wire(&QueryRequest::Selection { text: false }),
            r#"{"req":"selection"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Selection { text: true }),
            r#"{"req":"selection","text":true}"#
        );
        assert_eq!(wire(&QueryRequest::Buffers), r#"{"req":"buffers"}"#);
        assert_eq!(wire(&QueryRequest::Layout), r#"{"req":"layout"}"#);
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
        assert_eq!(
            wire(&QueryRequest::Goto {
                path: PathBuf::from("/a.rs"),
                range: "2:1-3:2".into(),
            }),
            r#"{"req":"goto","path":"/a.rs","range":"2:1-3:2"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Act {
                action: "MoveDown".into(),
            }),
            r#"{"req":"act","action":"MoveDown"}"#
        );
        assert_eq!(
            wire(&QueryRequest::Keys {
                keys: "ihello<Esc>".into(),
            }),
            r#"{"req":"keys","keys":"ihello<Esc>"}"#
        );
        assert_eq!(wire(&QueryRequest::Watch), r#"{"req":"watch"}"#);
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

        assert_eq!(live_sockets(dir.path(), &LocalFs).unwrap(), [live]);
        assert!(!dead.exists(), "the dead socket stays in the state dir");
    }

    #[test]
    fn discovery_keeps_each_session_that_answers() {
        let dir = tempfile::tempdir().unwrap();
        let answering = dir.path().join("agent-1.sock");
        let reply = json!({
            "uid": "1",
            "name": "a",
            "git_root": "/repo",
            "active": true,
            "last_active_ms": 5,
            "pid": 7,
        });
        fake_session(&answering, Some(reply.clone()));
        fake_session(&dir.path().join("agent-2.sock"), None);

        assert_eq!(
            live_sessions(dir.path(), &LocalFs).unwrap(),
            [(
                answering,
                serde_json::from_value::<SessionInfo>(reply).unwrap()
            )],
        );
    }

    #[test]
    fn a_name_selects_its_session() {
        let sessions = [
            session("1", "brave-otter", "/a", true, 9),
            session("2", "calm-heron", "/b", false, 1),
        ];
        assert_eq!(
            pick_session(&sessions, Some("Calm-Heron"), Path::new("/a")).unwrap(),
            &sessions[1].0,
        );
    }

    #[test]
    fn the_cwd_picks_the_session_whose_root_holds_it() {
        let sessions = [
            session("1", "a", "/repo/a", true, 9),
            session("2", "b", "/repo/b", false, 1),
        ];
        assert_eq!(
            pick_session(&sessions, None, Path::new("/repo/b/src")).unwrap(),
            &sessions[1].0,
        );
    }

    #[test]
    fn the_active_session_wins_a_shared_root() {
        let sessions = [
            session("1", "a", "/repo", false, 9),
            session("2", "b", "/repo", true, 1),
        ];
        assert_eq!(
            pick_session(&sessions, None, Path::new("/repo")).unwrap(),
            &sessions[1].0,
        );
    }

    #[test]
    fn the_latest_session_wins_two_background_ones() {
        let sessions = [
            session("1", "a", "/repo", false, 1),
            session("2", "b", "/repo", false, 9),
        ];
        assert_eq!(
            pick_session(&sessions, None, Path::new("/repo")).unwrap(),
            &sessions[1].0,
        );
    }

    #[test]
    fn an_unmatched_cwd_falls_back_to_the_sole_session() {
        let sessions = [session("1", "a", "/repo", false, 1)];
        assert_eq!(
            pick_session(&sessions, None, Path::new("/elsewhere")).unwrap(),
            &sessions[0].0,
        );
    }

    #[test]
    fn an_unmatched_cwd_with_several_sessions_lists_them() {
        let sessions = [
            session("1", "a", "/repo/a", false, 9),
            session("2", "b", "/repo/b", false, 1),
        ];
        assert_eq!(
            pick_session(&sessions, None, Path::new("/elsewhere"))
                .unwrap_err()
                .to_string(),
            "several sessions are live and none holds /elsewhere, pass --session <ID>:\
             \n1  a  /repo/a\n2  b  /repo/b",
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
