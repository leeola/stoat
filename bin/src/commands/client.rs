//! Thin client: hand files to a running Stoat over the IPC socket.

use snafu::{OptionExt, ResultExt, Whatever};
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
pub fn open_in_running_app(files: &[PathBuf]) -> Result<bool, Whatever> {
    if files.is_empty() {
        return Ok(false);
    }
    let socket = stoat_log::app_socket_path().whatever_context("resolve app socket path")?;
    let cwd = std::env::current_dir().whatever_context("resolve current directory")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build client runtime")?;
    runtime.block_on(send_open_file(&socket, &cwd, files))
}

async fn send_open_file(socket: &Path, cwd: &Path, files: &[PathBuf]) -> Result<bool, Whatever> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(_) => return Ok(false),
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
        let params = serde_json::json!({ "cwd": cwd_str, "path": path });
        peer.request("open_file", Some(params))
            .await
            .whatever_context("open_file request failed")?;
    }
    Ok(true)
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
            !open_in_running_app(&[]).expect("empty file list is a no-op"),
            "no files means nothing to route, so the caller launches an app",
        );
    }
}
