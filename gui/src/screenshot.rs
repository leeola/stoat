use std::path::{Path, PathBuf};

/// Resolve a screenshot's output path.
///
/// An empty `arg` yields a timestamped default `screenshot-<millis>.png`
/// under `root`. A relative `arg` resolves under `root`; an absolute one is
/// used verbatim. `unix_millis` is passed in rather than read from the clock
/// so the resolution stays pure.
pub fn resolve_screenshot_path(arg: &str, root: &Path, unix_millis: u128) -> PathBuf {
    let arg = arg.trim();
    if arg.is_empty() {
        return root.join(format!("screenshot-{unix_millis}.png"));
    }

    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// A `screencapture` shell command capturing the screen region at global
/// `(x, y)` of size `(w, h)` in logical points to `path`.
///
/// `-x` mutes the capture; `-R` selects the rectangle, so the window needs no
/// CGWindowID (which gpui does not expose). The path is single-quoted so
/// spaces and shell metacharacters in it stay literal under `sh -c`.
pub fn screencapture_command(path: &Path, x: i32, y: i32, w: i32, h: i32) -> String {
    format!(
        "screencapture -x -R{x},{y},{w},{h} {}",
        shell_single_quote(&path.to_string_lossy())
    )
}

/// POSIX single-quote `s` for safe inclusion in a `sh -c` command: wrap in
/// single quotes and rewrite each embedded `'` as `'\''`.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{resolve_screenshot_path, screencapture_command};
    use std::path::{Path, PathBuf};

    #[test]
    fn empty_arg_uses_timestamped_default_under_root() {
        let path = resolve_screenshot_path("  ", Path::new("/work/root"), 1_718_203_822_123);
        assert_eq!(
            path,
            PathBuf::from("/work/root/screenshot-1718203822123.png")
        );
    }

    #[test]
    fn relative_arg_resolves_under_root() {
        let path = resolve_screenshot_path("shots/a.png", Path::new("/work/root"), 0);
        assert_eq!(path, PathBuf::from("/work/root/shots/a.png"));
    }

    #[test]
    fn absolute_arg_is_used_verbatim() {
        let path = resolve_screenshot_path("/tmp/a.png", Path::new("/work/root"), 0);
        assert_eq!(path, PathBuf::from("/tmp/a.png"));
    }

    #[test]
    fn command_builds_region_and_quoted_path() {
        let cmd = screencapture_command(Path::new("/tmp/a.png"), 10, 20, 800, 600);
        assert_eq!(cmd, "screencapture -x -R10,20,800,600 '/tmp/a.png'");
    }

    #[test]
    fn command_quotes_paths_with_spaces_and_quotes() {
        let cmd = screencapture_command(Path::new("/tmp/o'brien dir/a.png"), 0, 0, 1, 1);
        assert_eq!(
            cmd,
            "screencapture -x -R0,0,1,1 '/tmp/o'\\''brien dir/a.png'"
        );
    }
}
