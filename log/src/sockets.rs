//! Where a Unix socket binds when its preferred directory runs too long.
//!
//! A Unix socket address holds 104 bytes on macOS and 108 on Linux. A socket
//! under a long `XDG_STATE_HOME` or `HOME` does not fit, so its bind fails. Such
//! a socket moves to a directory of the user's own in the runtime directory.
//! Every process that binds the socket or reaches it by its name resolves that
//! directory the same way.

use std::{
    fs::{self, DirBuilder},
    io::{self, Error, ErrorKind},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::SocketAddr,
    },
    path::{Path, PathBuf},
};

/// The directory to hold the Unix socket called `name`.
///
/// It is `preferred` when a socket path there fits in a Unix socket address.
/// Otherwise it is a directory of this user's own in the runtime directory:
/// `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` elsewhere, and `/tmp` without either.
/// Creates nothing.
///
/// See also:
/// - [`socket_bind_dir`] to make the directory ready for a bind.
pub fn socket_dir(preferred: PathBuf, name: &str) -> PathBuf {
    socket_dir_in(preferred, name, runtime_dir(), user_id())
}

/// [`socket_dir`], ready for this process to bind the socket called `name` in.
///
/// A directory in the runtime directory is created private to this user. An
/// existing one is refused when another user owns it or other users have
/// access to it. Any user with access to the directory controls the sockets in
/// it. `preferred` is used as found.
pub fn socket_bind_dir(preferred: PathBuf, name: &str) -> io::Result<PathBuf> {
    let user = user_id();
    let dir = socket_dir_in(preferred.clone(), name, runtime_dir(), user);
    if dir != preferred {
        ensure_private_dir(&dir, user)?;
    }
    Ok(dir)
}

/// The directory [`socket_dir`] picks from the preferred directory, the
/// socket's name, the runtime directory, and the user id that names the
/// fallback.
fn socket_dir_in(preferred: PathBuf, name: &str, runtime: Option<PathBuf>, user: u32) -> PathBuf {
    if SocketAddr::from_pathname(preferred.join(name)).is_ok() {
        return preferred;
    }
    runtime
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("stoat-{user}"))
}

/// The runtime directory the environment names, `$TMPDIR` on macOS and
/// `$XDG_RUNTIME_DIR` elsewhere.
// The env read is the blessed boundary. It hands its value straight to
// socket_dir_in, which is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
fn runtime_dir() -> Option<PathBuf> {
    let var = if cfg!(target_os = "macos") {
        "TMPDIR"
    } else {
        "XDG_RUNTIME_DIR"
    };
    std::env::var_os(var).map(PathBuf::from)
}

/// The real user id of this process.
fn user_id() -> u32 {
    // SAFETY: `getuid` takes no arguments and always succeeds.
    unsafe { libc::getuid() }
}

/// Create `dir` private to `user`, or make sure that it is private already.
///
/// An existing directory passes when `user` owns it and no other user has
/// access to it. Anything else is a [`ErrorKind::PermissionDenied`] error.
fn ensure_private_dir(dir: &Path, user: u32) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {},
        created => return created,
    }
    let meta = fs::symlink_metadata(dir)?;
    if meta.is_dir() && meta.uid() == user && meta.mode() & 0o077 == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!("{} is not private to this user", dir.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{ensure_private_dir, socket_dir_in};
    use std::{
        fs::{self, Permissions},
        io::ErrorKind,
        os::unix::fs::{self as unix_fs, MetadataExt, PermissionsExt},
        path::PathBuf,
    };

    /// A socket with no room in the preferred directory moves to this user's
    /// own directory in the runtime directory, or in `/tmp` without one. A long
    /// name moves its socket from a short directory too.
    #[test]
    fn a_socket_with_no_room_moves_to_the_runtime_dir() {
        let long = PathBuf::from(format!("/{}", "s".repeat(100)));
        let short = PathBuf::from("/state");
        let runtime = Some(PathBuf::from("/run/user/7"));
        let long_name = format!("{}.sock", "n".repeat(110));
        assert_eq!(
            [
                socket_dir_in(short.clone(), "a.sock", runtime.clone(), 7),
                socket_dir_in(long.clone(), "a.sock", runtime.clone(), 7),
                socket_dir_in(long, "a.sock", None, 7),
                socket_dir_in(short, &long_name, runtime, 7),
            ],
            [
                PathBuf::from("/state"),
                PathBuf::from("/run/user/7/stoat-7"),
                PathBuf::from("/tmp/stoat-7"),
                PathBuf::from("/run/user/7/stoat-7"),
            ],
        );
    }

    /// Other users share the runtime directory, so a socket directory there is
    /// one this user creates private or finds private.
    #[test]
    fn a_runtime_socket_dir_is_private_to_its_user() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().metadata().unwrap().uid();
        let created = root.path().join("created");
        let open = root.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");

        let first = ensure_private_dir(&created, user);
        unix_fs::symlink(&created, &link).unwrap();
        let kinds = [
            first,
            ensure_private_dir(&created, user),
            ensure_private_dir(&created, user + 1),
            ensure_private_dir(&open, user),
            ensure_private_dir(&link, user),
        ]
        .map(|result| result.map_err(|err| err.kind()));
        let mode = created.metadata().unwrap().mode() & 0o777;

        assert_eq!(
            (kinds, mode),
            (
                [
                    Ok(()),
                    Ok(()),
                    Err(ErrorKind::PermissionDenied),
                    Err(ErrorKind::PermissionDenied),
                    Err(ErrorKind::PermissionDenied),
                ],
                0o700,
            ),
            "created, its own, another owner, open to others, a planted link",
        );
    }
}
