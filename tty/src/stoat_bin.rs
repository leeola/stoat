//! Resolving the stoat editor binary stoatty launches as its default child.
//!
//! stoatty is the GPU front-end for the CLI stoat editor, so a bare `stoatty`
//! should open the editor. The binary is resolved at runtime rather than baked
//! in at build time, so a package manager can point at it via `STOAT_BIN` or
//! co-locate it, and a user can override it with no rebuild.

use crate::config::Config;
use std::{ffi::OsString, path::PathBuf};

/// Resolve the stoat editor binary to launch as stoatty's default child.
///
/// The `STOAT_BIN` environment variable (a full binary path) wins first, then
/// the `stoat_program` config override, then the `stoat` binary beside the
/// running stoatty executable, and finally the bare name `stoat` resolved via
/// PATH at spawn.
pub fn resolve(config: &Config) -> PathBuf {
    resolve_with(std::env::var_os("STOAT_BIN"), config, sibling_stoat())
}

/// The `stoat` binary next to the running stoatty executable, when it exists on
/// disk. Covers `target/debug` in development and a co-located release bundle.
fn sibling_stoat() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok()?.parent()?.join("stoat");
    sibling.exists().then_some(sibling)
}

/// Apply the resolution precedence to already-gathered inputs, so it is
/// testable without reading the environment or filesystem.
fn resolve_with(env: Option<OsString>, config: &Config, sibling: Option<PathBuf>) -> PathBuf {
    if let Some(path) = env {
        return PathBuf::from(path);
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
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn env_var_wins_over_config_and_sibling() {
        let mut config = embedded_default();
        config.stoat_program = Some(PathBuf::from("/config/stoat"));
        let resolved = resolve_with(
            Some(OsString::from("/env/stoat")),
            &config,
            Some(PathBuf::from("/sibling/stoat")),
        );
        assert_eq!(resolved, PathBuf::from("/env/stoat"));
    }

    #[test]
    fn config_wins_over_sibling_when_env_unset() {
        let mut config = embedded_default();
        config.stoat_program = Some(PathBuf::from("/config/stoat"));
        let resolved = resolve_with(None, &config, Some(PathBuf::from("/sibling/stoat")));
        assert_eq!(resolved, PathBuf::from("/config/stoat"));
    }

    #[test]
    fn sibling_wins_when_env_and_config_unset() {
        let config = embedded_default();
        let resolved = resolve_with(None, &config, Some(PathBuf::from("/sibling/stoat")));
        assert_eq!(resolved, PathBuf::from("/sibling/stoat"));
    }

    #[test]
    fn bare_name_when_nothing_resolves() {
        let config = embedded_default();
        assert_eq!(resolve_with(None, &config, None), PathBuf::from("stoat"));
    }
}
