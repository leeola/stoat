//! Filesystem-entry completion source. Triggered when the cursor
//! sits on a path-shaped prefix; lists the parent directory via
//! [`crate::host::FsHost`] and produces a row per entry, with a
//! trailing `/` on directories so chained completions feel natural.

use crate::{
    completion::{CompletionContext, CompletionItem, CompletionItemKind, CompletionSource},
    host::FsHost,
};
use std::path::{Path, PathBuf};

/// Fetch path completion items for the current cursor context.
///
/// `base_dir` is the directory relative paths resolve against
/// (typically the focused buffer's directory or the workspace
/// root). `home_dir` enables `~/`-prefixed lookups when `Some`;
/// `None` falls through to a relative resolution.
///
/// Returns an empty `Vec` when the prefix is not path-shaped, when
/// the parent directory does not exist, or when the listing
/// fails. All IO is mediated by `fs`, so tests drive this with
/// `FakeFs`.
pub fn fetch(
    ctx: &CompletionContext<'_>,
    fs: &dyn FsHost,
    base_dir: &Path,
    home_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    let Some(suffix) = path_suffix(ctx.text_before_cursor) else {
        return Vec::new();
    };
    if !suffix.contains('/') && !suffix.starts_with('~') {
        return Vec::new();
    }

    let (parent_subpath, typed_name) = split_path_suffix(&suffix);
    let parent_dir = resolve_parent(parent_subpath, base_dir, home_dir);

    let entries = match fs.list_dir(&parent_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let typed_lc = typed_name.to_lowercase();
    let typed_byte_len = typed_name.len();
    let cursor = ctx.cursor_offset;
    let replace_start = cursor.saturating_sub(typed_byte_len);

    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.name.as_str();
        if !typed_lc.is_empty() && !name.to_lowercase().starts_with(&typed_lc) {
            continue;
        }
        let label = if entry.is_dir {
            format!("{name}/")
        } else {
            name.to_string()
        };
        items.push(CompletionItem {
            label: label.clone(),
            source: CompletionSource::Path,
            kind: Some(if entry.is_dir {
                CompletionItemKind::Folder
            } else {
                CompletionItemKind::File
            }),
            detail: None,
            replace_range: replace_start..cursor,
            insert_text: label,
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        });
    }
    items
}

/// Walk back from the end of `text` collecting characters that
/// look like part of a path. Returns the resulting suffix, or
/// `None` when no path-shaped chars are present.
fn path_suffix(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        if is_path_char(bytes[start - 1] as char) {
            start -= 1;
        } else {
            break;
        }
    }
    if start == bytes.len() {
        return None;
    }
    Some(text[start..].to_string())
}

fn is_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '~')
}

/// Split a path suffix on the last `/`. When the suffix ends with
/// a slash, the typed name is empty and the entire suffix names
/// the parent.
fn split_path_suffix(suffix: &str) -> (&str, &str) {
    if let Some(pos) = suffix.rfind('/') {
        (&suffix[..pos + 1], &suffix[pos + 1..])
    } else {
        ("", suffix)
    }
}

fn resolve_parent(parent_subpath: &str, base_dir: &Path, home_dir: Option<&Path>) -> PathBuf {
    if parent_subpath.is_empty() {
        return base_dir.to_path_buf();
    }
    if parent_subpath.starts_with('/') {
        return PathBuf::from(parent_subpath);
    }
    if let (Some(home), Some(rest)) = (home_dir, parent_subpath.strip_prefix("~/")) {
        return home.join(rest);
    }
    base_dir.join(parent_subpath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    fn ctx<'a>(text: &'a str) -> CompletionContext<'a> {
        CompletionContext {
            cursor_offset: text.len(),
            prefix: "",
            prefix_range: text.len()..text.len(),
            text_before_cursor: text,
        }
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn empty_text_returns_no_items() {
        let fs = FakeFs::new();
        let items = fetch(&ctx(""), &fs, Path::new("/ws"), None);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn bare_identifier_returns_no_items() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/foo.txt", b"");
        let items = fetch(&ctx("foo"), &fs, Path::new("/ws"), None);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn dot_slash_lists_base_dir_with_directory_suffix() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/main.rs", b"");
        fs.insert_file("/ws/lib.rs", b"");
        fs.insert_dir("/ws/src");
        let items = fetch(&ctx("./"), &fs, Path::new("/ws"), None);
        let mut got = labels(&items);
        got.sort();
        assert_eq!(got, vec!["lib.rs", "main.rs", "src/"]);
    }

    #[test]
    fn dot_slash_replace_range_is_zero_width_at_cursor() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/lib.rs", b"");
        let items = fetch(&ctx("./"), &fs, Path::new("/ws"), None);
        for item in &items {
            assert_eq!(item.replace_range, 2..2, "replace range follows cursor");
        }
    }

    #[test]
    fn nested_path_lists_subdirectory() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/sub/a.txt", b"");
        fs.insert_file("/ws/sub/b.txt", b"");
        let items = fetch(&ctx("./sub/"), &fs, Path::new("/ws"), None);
        let mut got = labels(&items);
        got.sort();
        assert_eq!(got, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn nested_path_with_typed_name_filters_by_prefix() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/sub/foo.txt", b"");
        fs.insert_file("/ws/sub/bar.txt", b"");
        fs.insert_file("/ws/sub/foobar.txt", b"");
        let items = fetch(&ctx("./sub/fo"), &fs, Path::new("/ws"), None);
        let mut got = labels(&items);
        got.sort();
        assert_eq!(got, vec!["foo.txt", "foobar.txt"]);
    }

    #[test]
    fn typed_name_replace_range_covers_only_typed_segment() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/sub/foo.txt", b"");
        let items = fetch(&ctx("./sub/fo"), &fs, Path::new("/ws"), None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].replace_range, 6..8, "replace covers only `fo`");
    }

    #[test]
    fn absolute_path_lists_root_directory() {
        let fs = FakeFs::new();
        fs.insert_file("/etc/hosts", b"");
        fs.insert_file("/etc/passwd", b"");
        let items = fetch(&ctx("/etc/hos"), &fs, Path::new("/ws"), None);
        let got = labels(&items);
        assert_eq!(got, vec!["hosts"]);
    }

    #[test]
    fn home_relative_path_uses_home_dir_when_provided() {
        let fs = FakeFs::new();
        fs.insert_dir("/home/u/Documents");
        fs.insert_file("/home/u/Downloads/x", b"");
        let items = fetch(
            &ctx("~/Doc"),
            &fs,
            Path::new("/ws"),
            Some(Path::new("/home/u")),
        );
        let got = labels(&items);
        assert_eq!(got, vec!["Documents/"]);
    }

    #[test]
    fn home_path_without_home_dir_falls_through_to_base() {
        let fs = FakeFs::new();
        fs.insert_dir("/ws/~/foo");
        let items = fetch(&ctx("~/fo"), &fs, Path::new("/ws"), None);
        let got = labels(&items);
        assert_eq!(got, vec!["foo/"]);
    }

    #[test]
    fn missing_directory_returns_empty() {
        let fs = FakeFs::new();
        let items = fetch(&ctx("./missing/"), &fs, Path::new("/ws"), None);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn case_insensitive_filter() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/Main.rs", b"");
        fs.insert_file("/ws/main.toml", b"");
        let items = fetch(&ctx("./MAIN"), &fs, Path::new("/ws"), None);
        let mut got = labels(&items);
        got.sort();
        assert_eq!(got, vec!["Main.rs", "main.toml"]);
    }
}
