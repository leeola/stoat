//! Path formatting for TUI display.
//!
//! Centralises the rule used wherever Stoat renders a path: show the tail
//! relative to a context directory, fall back to `~/<tail>` when the path is
//! under the user's home, and only print the absolute form when neither
//! prefix applies. A companion [`common_ancestor`] picks the deepest shared
//! directory across a set of paths so list-style displays (the workspace
//! picker) can strip the repetitive root once and show only the
//! distinguishing tails.

use etcetera::{base_strategy::Xdg, BaseStrategy};
use std::path::{Component, Path, PathBuf};

/// Render `path` for display, shortened against `context` when possible.
///
/// Precedence:
/// 1. If `path` is under `context`, return the relative tail. Equal paths return `"."`.
/// 2. Else if `path` is under the user's home directory, return `~/<tail>` (or `"~"` for the home
///    directory itself).
/// 3. Else return the path lossily decoded.
///
/// Relative `path` inputs pass through unchanged.
pub(crate) fn display_relative(path: &Path, context: &Path) -> String {
    display_relative_with_home(path, context, home_dir().as_deref())
}

/// Longest path-component ancestor shared by every path in `paths`.
///
/// Returns `None` when the iterator is empty, when any element is relative
/// (mixing absolute and relative paths cannot yield a meaningful common
/// prefix), or when the only shared prefix is the filesystem root.
///
/// Guarantees the ancestor is *strictly* a prefix of each input: if the
/// naive deepest prefix equals any element (e.g. single path, or all
/// identical), the result steps up to the parent so the tails each caller
/// computes are non-empty.
pub(crate) fn common_ancestor<'a, I>(paths: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = &'a Path>,
{
    let collected: Vec<&Path> = paths.into_iter().collect();
    if collected.is_empty() {
        return None;
    }
    for p in &collected {
        if !p.is_absolute() {
            return None;
        }
    }

    let mut ancestor: PathBuf = collected[0].to_path_buf();
    for p in &collected[1..] {
        while !p.starts_with(&ancestor) {
            if !ancestor.pop() {
                return None;
            }
        }
    }

    if collected.contains(&ancestor.as_path()) && !ancestor.pop() {
        return None;
    }

    if is_bare_root(&ancestor) {
        return None;
    }
    Some(ancestor)
}

/// Absolute path to the user's editable config file,
/// `config.stcfg` under the XDG config home (typically
/// `~/.config/stoat/config.stcfg`).
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

pub(crate) fn display_relative_with_home(
    path: &Path,
    context: &Path,
    home: Option<&Path>,
) -> String {
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
    if let Some(home) = home
        && let Ok(rel) = path.strip_prefix(home)
    {
        return if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.to_string_lossy())
        };
    }
    path.to_string_lossy().into_owned()
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    Xdg::new().ok().map(|x| x.home_dir().to_path_buf())
}

fn is_bare_root(p: &Path) -> bool {
    let mut comps = p.components();
    matches!(comps.next(), Some(Component::RootDir)) && comps.next().is_none()
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
    fn common_ancestor_empty_returns_none() {
        let paths: Vec<&Path> = vec![];
        assert_eq!(common_ancestor(paths), None);
    }

    #[test]
    fn common_ancestor_single_returns_parent() {
        assert_eq!(common_ancestor([p("/a/b/c")]), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_single_at_root_returns_none() {
        assert_eq!(common_ancestor([p("/a")]), None);
    }

    #[test]
    fn common_ancestor_two_shared() {
        assert_eq!(
            common_ancestor([p("/a/b/c"), p("/a/b/d")]),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn common_ancestor_three_shared_at_different_depths() {
        assert_eq!(
            common_ancestor([p("/a/b/c"), p("/a/b/d"), p("/a/b/e/f")]),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn common_ancestor_divergent_returns_none() {
        assert_eq!(common_ancestor([p("/a/b"), p("/x/y")]), None);
    }

    #[test]
    fn common_ancestor_mixed_abs_rel_returns_none() {
        assert_eq!(common_ancestor([p("/a/b"), p("x/y")]), None);
    }

    #[test]
    fn common_ancestor_identical_returns_parent() {
        assert_eq!(
            common_ancestor([p("/a/b/c"), p("/a/b/c")]),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn common_ancestor_one_contains_other_steps_up() {
        assert_eq!(
            common_ancestor([p("/a/b/c"), p("/a/b")]),
            Some(PathBuf::from("/a"))
        );
    }
}
