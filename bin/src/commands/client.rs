//! Thin client: hand files to a running Stoat over the IPC socket.

use snafu::{whatever, OptionExt, ResultExt, Whatever};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_agent_claude_code::jsonrpc::JsonRpcPeer;
use stoat_scheduler::TokioScheduler;
use tokio::net::UnixStream;

/// Hand `files` to a Stoat app already listening on the IPC socket.
///
/// Returns `Ok(true)` when a live app accepted every file -- the caller should
/// exit. Returns `Ok(false)` when there is nothing to send or no app is
/// listening, so the caller launches one instead. Errors only when a connected
/// app fails a request, surfacing a half-broken app rather than silently
/// relaunching over it.
pub fn open_in_running_app(
    files: &[PathBuf],
    new: bool,
    session: Option<u64>,
) -> Result<bool, Whatever> {
    if files.is_empty() {
        return Ok(false);
    }
    let socket = stoat_log::app_socket_path().whatever_context("resolve app socket path")?;
    let cwd = std::env::current_dir().whatever_context("resolve current directory")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build client runtime")?;
    runtime.block_on(send_open_file(&socket, &cwd, files, new, session))
}

async fn send_open_file(
    socket: &Path,
    cwd: &Path,
    files: &[PathBuf],
    new: bool,
    session: Option<u64>,
) -> Result<bool, Whatever> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(_) => {
            if let Some(id) = session {
                whatever!("--session {id}: no Stoat app is running");
            }
            return Ok(false);
        },
    };
    let cwd_str = cwd
        .to_str()
        .whatever_context("current directory is not valid UTF-8")?;
    let scheduler = Arc::new(TokioScheduler::new(tokio::runtime::Handle::current()));
    let (peer, _incoming) = JsonRpcPeer::connect_unix(stream, &scheduler.executor());
    for file in files {
        let absolute = absolute_path(file, cwd);
        let path = absolute
            .to_str()
            .whatever_context("file path is not valid UTF-8")?;
        let mut params = serde_json::json!({ "cwd": cwd_str, "path": path });
        if new {
            params["new"] = serde_json::Value::Bool(true);
        }
        if let Some(id) = session {
            params["session"] = serde_json::Value::from(id);
        }
        peer.request("open_file", Some(params))
            .await
            .whatever_context("open_file request failed")?;
    }
    Ok(true)
}

/// Hand piped `text` to a Stoat app already listening on the IPC socket,
/// seeding a scratch buffer.
///
/// Returns `Ok(true)` when a live app accepted it -- the caller should exit.
/// Returns `Ok(false)` when no app is listening, so the caller launches one
/// instead. Errors when a connected app fails the request, or when `--session`
/// targets a uid while no app is running.
pub fn pipe_to_running_app(text: &str, new: bool, session: Option<u64>) -> Result<bool, Whatever> {
    let socket = stoat_log::app_socket_path().whatever_context("resolve app socket path")?;
    let cwd = std::env::current_dir().whatever_context("resolve current directory")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build client runtime")?;
    runtime.block_on(send_pipe_buffer(&socket, &cwd, text, new, session))
}

async fn send_pipe_buffer(
    socket: &Path,
    cwd: &Path,
    text: &str,
    new: bool,
    session: Option<u64>,
) -> Result<bool, Whatever> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(_) => {
            if let Some(id) = session {
                whatever!("--session {id}: no Stoat app is running");
            }
            return Ok(false);
        },
    };
    let cwd_str = cwd
        .to_str()
        .whatever_context("current directory is not valid UTF-8")?;
    let scheduler = Arc::new(TokioScheduler::new(tokio::runtime::Handle::current()));
    let (peer, _incoming) = JsonRpcPeer::connect_unix(stream, &scheduler.executor());

    let mut params = serde_json::json!({ "cwd": cwd_str, "text": text });
    if new {
        params["new"] = serde_json::Value::Bool(true);
    }
    if let Some(id) = session {
        params["session"] = serde_json::Value::from(id);
    }
    peer.request("pipe_buffer", Some(params))
        .await
        .whatever_context("pipe_buffer request failed")?;
    Ok(true)
}

/// Read buffer `buffer_id`'s text from a running Stoat app. `session` targets a
/// session by uid; absent, the cwd-enclosing session is used.
///
/// Errors when no app is running, the session or buffer is unknown, or the
/// reply is malformed. A read has no launch fallback -- there is nothing to
/// read without a live app.
pub fn read_buffer_from_app(buffer_id: u64, session: Option<u64>) -> Result<String, Whatever> {
    let socket = stoat_log::app_socket_path().whatever_context("resolve app socket path")?;
    let cwd = std::env::current_dir().whatever_context("resolve current directory")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build client runtime")?;
    runtime.block_on(send_read_buffer(&socket, &cwd, buffer_id, session))
}

async fn send_read_buffer(
    socket: &Path,
    cwd: &Path,
    buffer_id: u64,
    session: Option<u64>,
) -> Result<String, Whatever> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(_) => whatever!("--buffer: no Stoat app is running"),
    };
    let cwd_str = cwd
        .to_str()
        .whatever_context("current directory is not valid UTF-8")?;
    let scheduler = Arc::new(TokioScheduler::new(tokio::runtime::Handle::current()));
    let (peer, _incoming) = JsonRpcPeer::connect_unix(stream, &scheduler.executor());

    let mut params = serde_json::json!({ "cwd": cwd_str, "buffer_id": buffer_id });
    if let Some(id) = session {
        params["session"] = serde_json::Value::from(id);
    }
    let result = peer
        .request("read_buffer", Some(params))
        .await
        .whatever_context("read_buffer request failed")?;
    let text = result
        .get("text")
        .and_then(serde_json::Value::as_str)
        .whatever_context("read_buffer reply missing text")?;
    Ok(text.to_string())
}

/// Resolve `path` against `cwd`, mirroring the editor's path handling: an
/// absolute path is used as-is, a relative one is joined onto `cwd`. Symlinks
/// are not resolved.
fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{absolute_path, open_in_running_app};
    use std::path::{Path, PathBuf};

    #[test]
    fn absolute_path_joins_relative_and_keeps_absolute() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            absolute_path(Path::new("src/main.rs"), cwd),
            PathBuf::from("/home/user/project/src/main.rs"),
        );
        assert_eq!(
            absolute_path(Path::new("/etc/hosts"), cwd),
            PathBuf::from("/etc/hosts"),
        );
    }

    #[test]
    fn no_files_is_a_no_op() {
        assert!(
            !open_in_running_app(&[], false, None).expect("empty file list is a no-op"),
            "no files means nothing to route, so the caller launches an app",
        );
    }
}
