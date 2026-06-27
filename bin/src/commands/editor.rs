use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

/// Open `file` in the owning Stoat instance and block until it closes.
///
/// Connects to the agent socket named by `STOAT_AGENT_SOCK`, sends an
/// open-editor request, and waits for the `editor-closed` reply before
/// returning, honoring the `$EDITOR <file>` contract so an owned agent's editor
/// edits land in the IDE. A connection that closes without a reply (the parent
/// instance exited) also returns, so the agent's editor is never left hanging.
pub fn run(file: PathBuf) -> Result<(), Whatever> {
    let socket_path = resolve_socket_path(std::env::var("STOAT_AGENT_SOCK").ok())?;
    let mut stream =
        UnixStream::connect(&socket_path).whatever_context("connect to agent socket")?;
    let line = request_line(&file)?;
    stream
        .write_all(line.as_bytes())
        .whatever_context("write open-editor request")?;
    stream
        .write_all(b"\n")
        .whatever_context("write open-editor terminator")?;
    wait_for_close(stream)
}

/// Resolve the agent socket from the spawn-injected `STOAT_AGENT_SOCK`.
///
/// The owned agent inherits this variable, so a bare `$EDITOR <file>`
/// invocation reaches its parent with no extra arguments. Without it there is
/// no parent instance to open the file in.
fn resolve_socket_path(env_sock: Option<String>) -> Result<PathBuf, Whatever> {
    match env_sock {
        Some(path) => Ok(PathBuf::from(path)),
        None => whatever!("no parent session: STOAT_AGENT_SOCK is unset"),
    }
}

/// Build the newline-free open-editor request line for `file`.
fn request_line(file: &Path) -> Result<String, Whatever> {
    let request = serde_json::json!({
        "req": "open-editor",
        "path": file.to_string_lossy().into_owned(),
    });
    serde_json::to_string(&request).whatever_context("serialize open-editor request")
}

/// Block until the instance reports the editor closed.
///
/// Returns on the first `editor-closed` reply. A closed connection with no such
/// reply also returns, so a parent that exits never leaves the agent's editor
/// hanging.
fn wait_for_close(stream: UnixStream) -> Result<(), Whatever> {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.whatever_context("read open-editor reply")?;
        if reply_is_editor_closed(&line) {
            return Ok(());
        }
    }
    Ok(())
}

/// True when `line` is the instance's `editor-closed` reply.
fn reply_is_editor_closed(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("reply").and_then(|reply| reply.as_str()) == Some("editor-closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_line_carries_open_editor_and_path() {
        let line = request_line(Path::new("/tmp/x")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["req"].as_str(), Some("open-editor"));
        assert_eq!(value["path"].as_str(), Some("/tmp/x"));
    }

    #[test]
    fn missing_socket_env_is_an_error() {
        assert!(resolve_socket_path(None).is_err());
    }

    #[test]
    fn socket_env_used_directly() {
        assert_eq!(
            resolve_socket_path(Some("/run/agent.sock".into())).unwrap(),
            PathBuf::from("/run/agent.sock"),
        );
    }

    #[test]
    fn only_editor_closed_reply_unblocks() {
        assert!(reply_is_editor_closed(r#"{"reply":"editor-closed"}"#));
        assert!(!reply_is_editor_closed(r#"{"reply":"other"}"#));
        assert!(!reply_is_editor_closed(""));
        assert!(!reply_is_editor_closed("not json"));
        assert!(!reply_is_editor_closed(r#"{"hook":"stop"}"#));
    }
}
