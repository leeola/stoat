//! Resolving the stoat editor binary stoatty launches as its default child.
//!
//! stoatty is the GPU front-end for the CLI stoat editor, so a bare `stoatty`
//! should open the editor. The binary is resolved at runtime rather than baked
//! in at build time, so a package manager can point at it via `STOAT_BIN` or
//! co-locate it, and a user can override it with no rebuild.

use crate::config::Config;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Resolve the stoat editor binary to launch as stoatty's default child.
///
/// The `STOAT_BIN` environment variable wins first, then the `stoat_program`
/// config override, then the `stoat` binary beside the running stoatty
/// executable, and finally the bare name `stoat` resolved via PATH at spawn.
///
/// A relative `STOAT_BIN` path such as `./target/debug/stoat` resolves against
/// stoatty's working directory, as a shell resolves it, and not against the
/// child's. A bare name such as `stoat-dev` stays a PATH search at spawn.
// The env, cwd, and exe reads are the blessed boundary. They hand their values
// to resolve_with, which applies the precedence and is unit-tested.
#[allow(clippy::disallowed_methods)]
pub fn resolve(config: &Config) -> PathBuf {
    resolve_with(
        std::env::var_os("STOAT_BIN"),
        std::env::current_dir().ok().as_deref(),
        config,
        sibling_stoat(),
    )
}

/// The `stoat` binary next to the running stoatty executable, when it exists on
/// disk. Covers `target/debug` in development and a co-located release bundle.
fn sibling_stoat() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok()?.parent()?.join("stoat");
    sibling.exists().then_some(sibling)
}

/// Apply the resolution precedence to already-gathered inputs, so it is
/// testable without reading the environment or filesystem.
fn resolve_with(
    env: Option<OsString>,
    cwd: Option<&Path>,
    config: &Config,
    sibling: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = env {
        let path = PathBuf::from(path);
        let has_directory = path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
        return match cwd {
            Some(cwd) if path.is_relative() && has_directory => cwd.join(path),
            _ => path,
        };
    }
    if let Some(path) = &config.stoat_program {
        return path.clone();
    }
    if let Some(path) = sibling {
        return path;
    }
    PathBuf::from("stoat")
}

#[cfg(test)]
mod tests {
    use super::resolve_with;
    use crate::config::embedded_default;
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
    };

    #[test]
    fn env_var_wins_over_config_and_sibling() {
        let mut config = embedded_default();
        config.stoat_program = Some(PathBuf::from("/config/stoat"));
        let resolved = resolve_with(
            Some(OsString::from("/env/stoat")),
            None,
            &config,
            Some(PathBuf::from("/sibling/stoat")),
        );
        assert_eq!(resolved, PathBuf::from("/env/stoat"));
    }

    #[test]
    fn config_wins_over_sibling_when_env_unset() {
        let mut config = embedded_default();
        config.stoat_program = Some(PathBuf::from("/config/stoat"));
        let resolved = resolve_with(None, None, &config, Some(PathBuf::from("/sibling/stoat")));
        assert_eq!(resolved, PathBuf::from("/config/stoat"));
    }

    #[test]
    fn sibling_wins_when_env_and_config_unset() {
        let config = embedded_default();
        let resolved = resolve_with(None, None, &config, Some(PathBuf::from("/sibling/stoat")));
        assert_eq!(resolved, PathBuf::from("/sibling/stoat"));
    }

    #[test]
    fn bare_name_when_nothing_resolves() {
        let config = embedded_default();
        assert_eq!(
            resolve_with(None, None, &config, None),
            PathBuf::from("stoat")
        );
    }

    #[test]
    fn relative_env_path_joins_the_working_directory() {
        let config = embedded_default();
        let resolve = |env: &str, cwd: Option<&str>| {
            resolve_with(Some(OsString::from(env)), cwd.map(Path::new), &config, None)
        };
        assert_eq!(
            [
                resolve("./target/debug/stoat", Some("/work")),
                resolve("bin/stoat", Some("/work")),
                resolve("stoat-dev", Some("/work")),
                resolve("/env/stoat", Some("/work")),
                resolve("./target/debug/stoat", None),
            ],
            [
                PathBuf::from("/work/./target/debug/stoat"),
                PathBuf::from("/work/bin/stoat"),
                PathBuf::from("stoat-dev"),
                PathBuf::from("/env/stoat"),
                PathBuf::from("./target/debug/stoat"),
            ],
        );
    }
}
