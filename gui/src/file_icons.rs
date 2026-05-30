//! File-type icon lookups (Nerd Font glyph + tint) keyed by path, for
//! the path-rendering pickers and the project tree.

use crate::theme::ThemeColors;
use gpui::{rgb, Hsla};
use std::path::Path;

const DIRECTORY: &str = "\u{f07b}";
const FILE: &str = "\u{f15b}";
const RUST: &str = "\u{e7a8}";
const TOML: &str = "\u{e6b2}";
const MARKDOWN: &str = "\u{e73e}";
const JS_TS: &str = "\u{e74e}";
const PYTHON: &str = "\u{e73c}";
const GO: &str = "\u{e724}";
const C_FAMILY: &str = "\u{e61e}";
const JSON: &str = "\u{e60b}";
const YAML: &str = "\u{e615}";
const SHELL: &str = "\u{f489}";
const HTML: &str = "\u{e736}";
const CSS: &str = "\u{e749}";
const LOCK: &str = "\u{f023}";
const DOCKERFILE: &str = "\u{e7b0}";
const MAKEFILE: &str = "\u{e673}";
const NODE: &str = "\u{e718}";

/// Recognized file categories, the single classification both
/// [`icon_for_path`] and [`color_for_path`] map from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Directory,
    Rust,
    Toml,
    Markdown,
    JsTs,
    Python,
    Go,
    CFamily,
    Json,
    Yaml,
    Shell,
    Html,
    Css,
    Lock,
    Dockerfile,
    Makefile,
    Node,
    Other,
}

/// Nerd Font glyph for `path`. Directories take precedence, then exact
/// filenames (Dockerfile, Cargo.lock, package.json, Makefile), then the
/// extension; unrecognized files get a generic file glyph.
pub fn icon_for_path(path: &Path, is_dir: bool) -> &'static str {
    match classify(path, is_dir) {
        FileKind::Directory => DIRECTORY,
        FileKind::Rust => RUST,
        FileKind::Toml => TOML,
        FileKind::Markdown => MARKDOWN,
        FileKind::JsTs => JS_TS,
        FileKind::Python => PYTHON,
        FileKind::Go => GO,
        FileKind::CFamily => C_FAMILY,
        FileKind::Json => JSON,
        FileKind::Yaml => YAML,
        FileKind::Shell => SHELL,
        FileKind::Html => HTML,
        FileKind::Css => CSS,
        FileKind::Lock => LOCK,
        FileKind::Dockerfile => DOCKERFILE,
        FileKind::Makefile => MAKEFILE,
        FileKind::Node => NODE,
        FileKind::Other => FILE,
    }
}

/// Tint for `path`'s icon. Recognized types return a fixed brand color;
/// unrecognized files fall back to `theme.muted_text` so they blend with
/// the surrounding chrome.
pub fn color_for_path(path: &Path, theme: &ThemeColors) -> Hsla {
    match classify(path, false) {
        FileKind::Rust => rgb(0xe06c2f).into(),
        FileKind::Toml => rgb(0x9c6b3f).into(),
        FileKind::Markdown => rgb(0x519aba).into(),
        FileKind::JsTs => rgb(0xf0db4f).into(),
        FileKind::Python => rgb(0x4b8bbe).into(),
        FileKind::Go => rgb(0x00add8).into(),
        FileKind::CFamily => rgb(0x659ad2).into(),
        FileKind::Json => rgb(0xcbcb41).into(),
        FileKind::Yaml => rgb(0xcb4b16).into(),
        FileKind::Shell => rgb(0x89e051).into(),
        FileKind::Html => rgb(0xe44d26).into(),
        FileKind::Css => rgb(0x563d7c).into(),
        FileKind::Lock => rgb(0x8b8b8b).into(),
        FileKind::Dockerfile => rgb(0x2496ed).into(),
        FileKind::Makefile => rgb(0x6d8086).into(),
        FileKind::Node => rgb(0x83cd29).into(),
        FileKind::Directory | FileKind::Other => theme.muted_text,
    }
}

fn classify(path: &Path, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Directory;
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        match name {
            "Dockerfile" => return FileKind::Dockerfile,
            "Cargo.lock" => return FileKind::Lock,
            "package.json" => return FileKind::Node,
            "Makefile" => return FileKind::Makefile,
            _ => {},
        }
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => FileKind::Rust,
        Some("toml") => FileKind::Toml,
        Some("md") => FileKind::Markdown,
        Some("js" | "jsx" | "ts" | "tsx") => FileKind::JsTs,
        Some("py") => FileKind::Python,
        Some("go") => FileKind::Go,
        Some("c" | "h" | "cpp" | "cc" | "hpp") => FileKind::CFamily,
        Some("json") => FileKind::Json,
        Some("yaml" | "yml") => FileKind::Yaml,
        Some("sh" | "bash") => FileKind::Shell,
        Some("html") => FileKind::Html,
        Some("css") => FileKind::Css,
        Some("lock") => FileKind::Lock,
        _ => FileKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ActiveTheme;
    use gpui::TestAppContext;

    #[test]
    fn icon_matches_directory_filename_and_extension() {
        assert_eq!(icon_for_path(Path::new("foo.rs"), false), RUST);
        assert_eq!(icon_for_path(Path::new("Cargo.lock"), false), LOCK);
        assert_ne!(
            icon_for_path(Path::new("Cargo.lock"), false),
            TOML,
            "the lock filename must win over the toml glyph"
        );
        assert_eq!(icon_for_path(Path::new("subdir"), true), DIRECTORY);
        assert_eq!(icon_for_path(Path::new("notes.xyz"), false), FILE);
    }

    #[test]
    fn color_is_branded_for_known_and_muted_for_unknown() {
        let cx = TestAppContext::single();
        let theme = cx.read(|cx| cx.theme());
        assert_eq!(
            color_for_path(Path::new("notes.xyz"), &theme),
            theme.muted_text
        );
        assert_ne!(
            color_for_path(Path::new("foo.rs"), &theme),
            theme.muted_text
        );
    }
}
