//! Path formatting for human display.
//!
//! Centralises the rule used wherever Stoat renders a path: show the tail
//! relative to a context directory, fall back to `~/<tail>` when the path is
//! under the user's home, and only print the absolute form when neither
//! prefix applies.
//!
//! See also `agent::claude_code::tools::to_display_path`, which performs a
//! simpler variant of the same strip for Claude tool-call titles. The two
//! live in different crates today (stoat depends on claude_code, not the
//! reverse); unify into a shared crate only if a third consumer appears.

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
/// Relative `path` inputs pass through unchanged.
pub fn display_relative(path: &Path, context: &Path) -> String {
    display_relative_with_home(path, context, home_dir().as_deref())
}

fn display_relative_with_home(path: &Path, context: &Path, home: Option<&Path>) -> String {
    if !path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    if let Ok(rel) = path.strip_prefix(context) {
        return if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.to_string_lossy().into_owned()
        };
    }
    if let Some(home) = home {
        if let Ok(rel) = path.strip_prefix(home) {
            return if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rel.to_string_lossy())
            };
        }
    }
    path.to_string_lossy().into_owned()
}

fn home_dir() -> Option<PathBuf> {
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
}
