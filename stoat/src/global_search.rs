use crate::host::FsHost;
use regex::Regex;
use std::path::{Path, PathBuf};

/// One match site surfaced by [`perform_search`].
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

const SNIPPET_MAX_CHARS: usize = 80;

/// Walk every workspace file via `fs_host`, scan for `pattern`, and
/// return one [`SearchMatch`] per match site. Skips files whose
/// contents are not valid UTF-8. Returns the compiled regex error
/// when `pattern` does not parse.
pub fn perform_search(
    fs_host: &dyn FsHost,
    git_root: &Path,
    pattern: &str,
) -> Result<Vec<SearchMatch>, regex::Error> {
    let regex = Regex::new(pattern)?;
    let mut matches = Vec::new();
    for path in fs_host.walk_workspace_files(git_root) {
        let mut buf = Vec::new();
        if fs_host.read(&path, &mut buf).is_err() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        for m in regex.find_iter(text) {
            let start = m.start();
            let (line, column) = offset_to_line_column(text, start);
            let snippet = line_snippet(text, start);
            matches.push(SearchMatch {
                path: path.clone(),
                offset: start,
                line,
                column,
                snippet,
            });
        }
    }
    Ok(matches)
}

/// Convert a byte offset into a `(line, column)` pair, both 1-based,
/// counting characters (not bytes) for the column. Out-of-range
/// `offset` clamps to the text length.
fn offset_to_line_column(text: &str, offset: usize) -> (u32, u32) {
    let clipped = offset.min(text.len());
    let preceding = &text[..clipped];
    let line = preceding.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let line_start = preceding.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = text[line_start..clipped].chars().count() as u32 + 1;
    (line, column)
}

/// Extract the line containing `offset`, trim leading whitespace, and
/// cap at [`SNIPPET_MAX_CHARS`] for compact display.
fn line_snippet(text: &str, offset: usize) -> String {
    let clipped = offset.min(text.len());
    let line_start = text[..clipped].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[clipped..]
        .find('\n')
        .map(|i| clipped + i)
        .unwrap_or(text.len());
    let raw = &text[line_start..line_end];
    raw.trim_start().chars().take(SNIPPET_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    fn fake_with(files: &[(&str, &str)]) -> (FakeFs, PathBuf) {
        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        (fs, root)
    }

    #[test]
    fn offset_to_line_column_first_line() {
        assert_eq!(offset_to_line_column("hello\nworld\n", 0), (1, 1));
        assert_eq!(offset_to_line_column("hello\nworld\n", 3), (1, 4));
    }

    #[test]
    fn offset_to_line_column_second_line() {
        assert_eq!(offset_to_line_column("hello\nworld\n", 6), (2, 1));
        assert_eq!(offset_to_line_column("hello\nworld\n", 8), (2, 3));
    }

    #[test]
    fn offset_to_line_column_counts_characters_for_column() {
        // "café" has c-a-f-é where é is 2 bytes (\xc3\xa9). Offset 5 is
        // after é, which is the 4th character on the line.
        assert_eq!(offset_to_line_column("café\n", 5), (1, 5));
    }

    #[test]
    fn line_snippet_returns_full_line_trimmed() {
        assert_eq!(line_snippet("    hello\nbeta\n", 4), "hello");
        assert_eq!(line_snippet("alpha\n  beta\n", 8), "beta");
    }

    #[test]
    fn perform_search_finds_matches_across_files() {
        let (fs, root) = fake_with(&[
            ("a.rs", "fn alpha() {}\nfn beta() {}\n"),
            ("b.rs", "fn alpha() {}\n"),
        ]);
        let matches = perform_search(&fs, &root, "alpha").unwrap();
        assert_eq!(matches.len(), 2);
        let paths: Vec<&str> = matches
            .iter()
            .map(|m| m.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(paths.contains(&"a.rs"));
        assert!(paths.contains(&"b.rs"));
        for m in &matches {
            assert_eq!(m.line, 1);
            assert_eq!(m.column, 4);
            assert_eq!(m.snippet, "fn alpha() {}");
        }
    }

    #[test]
    fn perform_search_with_invalid_regex_returns_err() {
        let (fs, root) = fake_with(&[("a.rs", "x")]);
        assert!(perform_search(&fs, &root, "[unclosed").is_err());
    }

    #[test]
    fn perform_search_skips_non_utf8_files() {
        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");
        fs.insert_file(root.join("good.rs"), b"hello");
        fs.insert_file(root.join("bad.bin"), [0xff, 0xfe, 0xfd]);
        let matches = perform_search(&fs, &root, "hello").unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.ends_with("good.rs"));
    }

    #[test]
    fn perform_search_empty_pattern_compiles_and_matches_every_position() {
        let (fs, root) = fake_with(&[("a.rs", "ab")]);
        let matches = perform_search(&fs, &root, "").unwrap();
        assert!(!matches.is_empty(), "empty regex should match");
    }
}
