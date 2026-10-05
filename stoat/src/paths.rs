//! Path formatting for TUI display.
//!
//! Centralises the rule used wherever Stoat renders a path: show the tail
//! relative to a context directory, fall back to `~/<tail>` when the path is
//! under the user's home, and only print the absolute form when neither
//! prefix applies.

use etcetera::{base_strategy::Xdg, BaseStrategy};
use std::path::{Path, PathBuf};

/// Render `path` for display, shortened against `context` when possible.
///
/// Precedence:
/// 1. If `path` is under `context`, return the relative tail. Equal paths return `"."`.
/// 2. Else if `path` is under the user's home directory, return `~/<tail>` (or `"~"` for the home
///    directory itself).
/// 3. Else return the path lossily decoded.
///
/// Relative `path` inputs pass through unchanged. An empty `context` contains
/// no path, so rules 2 and 3 decide.
pub(crate) fn display_relative(path: &Path, context: &Path) -> String {
    display_relative_with_home(path, context, home_dir().as_deref())
}

/// Absolute path to the user's editable config file,
/// `config.stcfg` under the XDG config home (typically
/// `~/.config/stoat/config.stcfg`).
///
/// The file layers over the embedded default rather than replacing it, so it
/// needs to state only what the user changes. Every key, setting, and theme it
/// leaves alone keeps the shipped value.
///
/// Returns [`None`] when the base-directory strategy cannot resolve a
/// config home, in which case startup falls back to the embedded
/// default config. The path is not guaranteed to exist. Callers read
/// it opportunistically.
pub fn user_config_path() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|x| x.config_dir().join("stoat/config.stcfg"))
}

/// Absolute path to stoatty's config file, `config.toml` under the XDG config
/// home (typically `~/.config/stoatty/config.toml`).
///
/// stoat resolves the terminal's config through the same base-directory
/// strategy stoatty itself uses, which holds because stoat runs as stoatty's
/// local child and inherits its environment. Over ssh this names the remote
/// host's file rather than the terminal's, so a caller acting on it is
/// addressing the wrong machine.
///
/// Returns [`None`] when no config home resolves. The path is not guaranteed to
/// exist.
pub(crate) fn stoatty_config_path() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|x| x.config_dir().join("stoatty/config.toml"))
}

/// The directory VSCode theme JSON files are read from, shared with stoatty so
/// one drop point serves both apps.
pub fn user_themes_dir() -> Option<PathBuf> {
    Xdg::new().ok().map(|x| x.config_dir().join("stoat/themes"))
}

pub(crate) fn display_relative_with_home(
    path: &Path,
    context: &Path,
    home: Option<&Path>,
) -> String {
    let mut out = String::new();
    write_display_relative_with_home(&mut out, path, context, home);
    out
}

/// Append [`display_relative_with_home`]'s text for `path` to `out`.
///
/// For a caller resolving one path per row of a paint, where a returned `String`
/// would allocate per row.
pub(crate) fn write_display_relative_with_home(
    out: &mut String,
    path: &Path,
    context: &Path,
    home: Option<&Path>,
) {
    if !path.is_absolute() {
        out.push_str(&path.to_string_lossy());
        return;
    }
    // `strip_prefix` accepts an empty base for every path and returns the path
    // whole, which is no relative tail. The home rule decides in that case.
    if !context.as_os_str().is_empty()
        && let Ok(rel) = path.strip_prefix(context)
    {
        match rel.as_os_str().is_empty() {
            true => out.push('.'),
            false => out.push_str(&rel.to_string_lossy()),
        }
        return;
    }
    if let Some(home) = home
        && let Ok(rel) = path.strip_prefix(home)
    {
        out.push('~');
        if !rel.as_os_str().is_empty() {
            out.push('/');
            out.push_str(&rel.to_string_lossy());
        }
        return;
    }
    out.push_str(&path.to_string_lossy());
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    Xdg::new().ok().map(|x| x.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn display_relative_strips_context_prefix() {
        let out = display_relative_with_home(p("/a/b/c/f.rs"), p("/a/b"), Some(p("/home/lee")));
        assert_eq!(out, "c/f.rs");
    }

    #[test]
    fn display_relative_equal_returns_dot() {
        let out = display_relative_with_home(p("/a/b"), p("/a/b"), Some(p("/home/lee")));
        assert_eq!(out, ".");
    }

    #[test]
    fn display_relative_falls_back_to_tilde() {
        let out = display_relative_with_home(p("/home/lee/src/x"), p("/tmp"), Some(p("/home/lee")));
        assert_eq!(out, "~/src/x");
    }

    #[test]
    fn display_relative_home_itself_is_tilde() {
        let out = display_relative_with_home(p("/home/lee"), p("/tmp"), Some(p("/home/lee")));
        assert_eq!(out, "~");
    }

    #[test]
    fn display_relative_absolute_fallback() {
        let out = display_relative_with_home(p("/etc/hosts"), p("/tmp"), Some(p("/home/lee")));
        assert_eq!(out, "/etc/hosts");
    }

    #[test]
    fn display_relative_no_home_falls_back_to_absolute() {
        let out = display_relative_with_home(p("/etc/hosts"), p("/tmp"), None);
        assert_eq!(out, "/etc/hosts");
    }

    #[test]
    fn display_relative_relative_input_passthrough() {
        let out = display_relative_with_home(p("foo/bar"), p("/home/lee"), Some(p("/home/lee")));
        assert_eq!(out, "foo/bar");
    }

    #[test]
    fn display_relative_empty_context_falls_back_to_tilde() {
        let out = display_relative_with_home(p("/home/lee/src/x"), p(""), Some(p("/home/lee")));
        assert_eq!(out, "~/src/x");
    }
}
