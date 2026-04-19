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
