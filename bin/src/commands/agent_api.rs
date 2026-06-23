use clap::Subcommand;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use stoat::{agent_status::AgentHookEvent, run, workspace::WorkspaceUid};

/// Subcommands for the thin client the owned Claude subshell's hooks invoke to
/// push status into the owning session.
#[derive(Subcommand, Debug)]
pub enum AgentApiCommand {
    /// Send one hook event to the owning session's IPC socket and exit.
    Hook {
        /// One of session-start, pre-tool-use, post-tool-use, notification,
        /// stop, or session-end.
        name: String,

        /// Extra values for the hook. pre-tool-use takes the tool name as its
        /// first value. Other hooks take none.
        values: Vec<String>,

        /// Target session uid (16-hex, as in STOAT_SESSION). Defaults to the
        /// socket path in the STOAT_AGENT_SOCK env var injected at spawn.
        #[arg(long)]
        session: Option<String>,
    },
}

pub fn run(sub: AgentApiCommand) -> Result<(), Whatever> {
    match sub {
        AgentApiCommand::Hook {
            name,
            values,
            session,
        } => hook(&name, &values, session.as_deref()),
    }
}

fn hook(name: &str, values: &[String], session: Option<&str>) -> Result<(), Whatever> {
    let event = build_event(name, values)?;
    let env_sock = std::env::var("STOAT_AGENT_SOCK").ok();
    let socket_path = resolve_socket_path(session, env_sock)?;
    send_event(&socket_path, &event)
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

/// Resolve which session socket to send to.
///
/// An explicit `session` uid wins and maps to its [`run::agent_socket_path`].
/// Otherwise `env_sock` (the spawn-injected `STOAT_AGENT_SOCK`) is the path
/// directly. With neither, there is no target to reach.
fn resolve_socket_path(
    session: Option<&str>,
    env_sock: Option<String>,
) -> Result<PathBuf, Whatever> {
    if let Some(session) = session {
        let uid = parse_session_uid(session)?;
        return run::agent_socket_path(uid).whatever_context("resolve agent socket path");
    }
    match env_sock {
        Some(path) => Ok(PathBuf::from(path)),
        None => whatever!("no target session: pass --session or set STOAT_AGENT_SOCK"),
    }
}

fn parse_session_uid(session: &str) -> Result<WorkspaceUid, Whatever> {
    let raw = u64::from_str_radix(session.trim_start_matches("0x"), 16)
        .whatever_context(format!("parse session uid {session:?}"))?;
    Ok(WorkspaceUid(raw))
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

    fn wire(name: &str, values: &[&str]) -> String {
        let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        serde_json::to_string(&build_event(name, &values).unwrap()).unwrap()
    }

    #[test]
    fn builds_each_hook_to_wire_form() {
        assert_eq!(wire("session-start", &[]), r#"{"hook":"session-start"}"#);
        assert_eq!(
            wire("pre-tool-use", &["Bash"]),
            r#"{"hook":"pre-tool-use","tool":"Bash"}"#
        );
        assert_eq!(wire("post-tool-use", &[]), r#"{"hook":"post-tool-use"}"#);
        assert_eq!(wire("notification", &[]), r#"{"hook":"notification"}"#);
        assert_eq!(wire("stop", &[]), r#"{"hook":"stop"}"#);
        assert_eq!(wire("session-end", &[]), r#"{"hook":"session-end"}"#);
    }

    #[test]
    fn unknown_hook_and_missing_tool_are_errors() {
        assert!(build_event("bogus", &[]).is_err());
        assert!(build_event("pre-tool-use", &[]).is_err());
    }

    #[test]
    fn session_uid_round_trips_through_display() {
        let uid = WorkspaceUid(0xABCD);
        assert_eq!(parse_session_uid(&uid.to_string()).unwrap(), uid);
        assert_eq!(parse_session_uid("0xabcd").unwrap(), uid);
    }

    #[test]
    fn env_socket_used_when_no_session() {
        assert_eq!(
            resolve_socket_path(None, Some("/run/agent.sock".into())).unwrap(),
            PathBuf::from("/run/agent.sock"),
        );
    }

    #[test]
    fn missing_target_is_an_error() {
        assert!(resolve_socket_path(None, None).is_err());
    }
}
