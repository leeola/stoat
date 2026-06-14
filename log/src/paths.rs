use etcetera::{base_strategy::Xdg, BaseStrategy};
use std::{io, path::PathBuf};

/// Returns the base data directory for user-generated Stoat artifacts:
/// `<XDG_DATA_HOME>/stoat/`.
///
/// Callers append a subdirectory (e.g. `dumps`) and are responsible for
/// creating the directory via [`std::fs::create_dir_all`] before writing.
pub fn data_dir() -> io::Result<PathBuf> {
    let base = Xdg::new().ok().map(|x| x.data_dir()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve XDG data directory",
        )
    })?;
    Ok(base.join("stoat"))
}

/// Returns the base state directory for Stoat session artifacts:
/// `<XDG_STATE_HOME>/stoat/`.
///
/// State-vs-data distinction follows the XDG convention: logs, workspace
/// snapshots, and other ephemeral session state live here, while user-facing
/// artifacts like dump archives go in [`data_dir`]. Callers are responsible
/// for creating subdirectories via [`std::fs::create_dir_all`] before writing.
pub fn state_dir() -> io::Result<PathBuf> {
    let base = Xdg::new().ok().and_then(|x| x.state_dir()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve XDG state directory",
        )
    })?;
    Ok(base.join("stoat"))
}

/// Returns the directory holding per-workspace state snapshots:
/// `<XDG_STATE_HOME>/stoat/workspaces/`.
pub fn workspace_state_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("workspaces"))
}

/// Returns the path of the singleton app IPC socket that the running Stoat
/// process binds and thin clients connect to.
///
/// Prefers `<XDG_RUNTIME_DIR>/stoat/app.sock` -- the runtime dir is the XDG
/// home for sockets and is cleared on logout, so a stale socket cannot
/// outlive the session. Falls back to `<XDG_STATE_HOME>/stoat/app.sock` when
/// `XDG_RUNTIME_DIR` is unset (common on macOS). The caller is responsible
/// for creating the parent directory before binding.
pub fn app_socket_path() -> io::Result<PathBuf> {
    let base = match runtime_dir() {
        Some(dir) => dir,
        None => state_dir()?,
    };
    Ok(base.join("app.sock"))
}

/// Returns `<XDG_RUNTIME_DIR>/stoat/`, or `None` when `XDG_RUNTIME_DIR` is
/// unset. Unlike the data/state dirs this is intentionally fallible: the
/// runtime dir does not exist on every platform, and callers fall back to
/// the state dir.
fn runtime_dir() -> Option<PathBuf> {
    Xdg::new().ok()?.runtime_dir().map(|dir| dir.join("stoat"))
}
