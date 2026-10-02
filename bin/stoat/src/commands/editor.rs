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
///
/// A buffer left with unsaved edits exits the process with status 1, so the
/// caller treats the edit as abandoned.
// The env read is the blessed boundary. It hands the value straight to
// resolve_socket_path, which is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
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
    let status = wait_for_close(stream)?;
    if status != 0 {
        std::process::exit(status);
    }
    Ok(())
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

/// Block until the instance reports the editor closed, and return the exit
/// status the reply carries.
///
/// Returns on the first `editor-closed` reply. Its status is 1 when the buffer
/// was left with unsaved edits, so the caller treats the edit as abandoned. A
/// closed connection with no such reply returns 0, so a parent that exits never
/// leaves the agent's editor hanging.
fn wait_for_close(stream: UnixStream) -> Result<i32, Whatever> {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.whatever_context("read open-editor reply")?;
        if let Some(status) = reply_status(&line) {
            return Ok(status);
        }
    }
    Ok(0)
}

/// The exit status an `editor-closed` reply carries, or `None` for any other
/// line.
///
/// A reply with no `status` field comes from an instance that reports no
/// outcome, so it reads as 0.
fn reply_status(line: &str) -> Option<i32> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("reply").and_then(|reply| reply.as_str()) != Some("editor-closed") {
        return None;
    }
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Some(i32::try_from(status).unwrap_or(1))
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
    fn only_an_editor_closed_reply_carries_a_status() {
        assert_eq!(
            [
                r#"{"reply":"editor-closed"}"#,
                r#"{"reply":"other"}"#,
                "",
                "not json",
                r#"{"hook":"stop"}"#,
                r#"{"reply":"editor-closed","status":1}"#,
            ]
            .map(reply_status),
            [Some(0), None, None, None, None, Some(1)],
        );
    }
}
