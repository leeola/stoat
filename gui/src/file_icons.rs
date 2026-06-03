//! File-type icon lookups (glyph + tint) keyed by path, for the
//! path-rendering pickers and the project tree.

use crate::theme::ThemeColors;
use gpui::{rgb, Hsla};
use std::path::Path;

// `.SystemUIFont` carries no folder/document glyph: the Unicode
// pictographs (U+1F5C0/U+1F5CE) fall through to the LastResort font and
// render as tofu, while the emoji forms (U+1F4C1/U+1F4C4) resolve to the
// color-emoji font and ignore the per-kind tint. These SF-native
// geometric shapes render monochrome and honor `color_for_path`, so the
// glyph carries only a folder-vs-file distinction and color carries type.
const DIRECTORY: &str = "\u{25a0}";
const FILE: &str = "\u{2022}";

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

/// Glyph for `path`: a folder marker for directories, a generic file
/// marker otherwise. File-type distinction is carried by the tint from
/// [`color_for_path`], not the glyph, since `.SystemUIFont` has no
/// per-language icons.
pub fn icon_for_path(path: &Path, is_dir: bool) -> &'static str {
    match classify(path, is_dir) {
        FileKind::Directory => DIRECTORY,
        _ => FILE,
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
    fn icon_distinguishes_directory_from_file() {
        assert_ne!(DIRECTORY, FILE, "folder and file glyphs must differ");
        assert_eq!(icon_for_path(Path::new("subdir"), true), DIRECTORY);
        assert_eq!(icon_for_path(Path::new("foo.rs"), false), FILE);
        assert_eq!(icon_for_path(Path::new("Cargo.lock"), false), FILE);
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
