use crate::{
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    picker::Preview,
    workspace::Workspace,
};
use regex::Regex;
use std::path::{Path, PathBuf};
use stoat_scheduler::Executor;

/// Match sites the live scan streams before it stops, so a pattern matching most
/// of the workspace never overruns the list or the preview.
pub(crate) const MATCH_CAP: usize = 500;

/// One match site surfaced by the workspace scan.
///
/// Carries the file, the match's byte offset, its 1-based line and column, and a
/// trimmed snippet of the matched line.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

/// Live workspace code-search modal.
///
/// Typing streams regex matches into `matches` from a debounced blocking scan.
/// A preview pane shows the selected match's file scrolled to its line. Enter
/// opens the file at the match, Escape closes.
pub struct CodeSearchFinder {
    pub(crate) input: InputView,
    pub(crate) matches: Vec<SearchMatch>,
    pub(crate) selected: usize,
    pub(crate) preview: Preview,
    /// The query the current match set was scanned for. A render tick re-arms the
    /// scan only when the typed text differs from this, so a stable query never
    /// re-scans the workspace.
    pub(crate) last_query: Option<String>,
}

impl CodeSearchFinder {
    pub(crate) fn new(ws: &mut Workspace, executor: Executor) -> Self {
        let input = InputView::create(
            ws,
            executor.clone(),
            SubmitTarget::CodeSearch,
            "",
            "insert",
            1,
        );
        let preview = Preview::new(ws, executor);
        Self {
            input,
            matches: Vec::new(),
            selected: 0,
            preview,
            last_query: None,
        }
    }

    pub(crate) fn selected_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.selected)
    }

    /// Append a streamed batch, leaving the selection where it is so results
    /// filling in never move the highlight under the user.
    pub(crate) fn push_matches(&mut self, mut more: Vec<SearchMatch>) {
        self.matches.append(&mut more);
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.matches.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.preview.dispose(ws);
    }
}

const SNIPPET_MAX_CHARS: usize = 80;

/// Read `path` through `fs_host`, scan its UTF-8 text for `regex`, and push one
/// [`SearchMatch`] per match site onto `out`.
///
/// A file that fails to read, or whose bytes are not valid UTF-8, contributes
/// nothing, so the scan is total over an arbitrary workspace tree.
pub(crate) fn scan_file(
    fs_host: &dyn FsHost,
    regex: &Regex,
    path: &Path,
    out: &mut Vec<SearchMatch>,
) {
    let mut buf = Vec::new();
    if fs_host.read(path, &mut buf).is_err() {
        return;
    }
    let Ok(text) = std::str::from_utf8(&buf) else {
        return;
    };
    for m in regex.find_iter(text) {
        let start = m.start();
        let (line, column) = offset_to_line_column(text, start);
        let snippet = line_snippet(text, start);
        out.push(SearchMatch {
            path: path.to_path_buf(),
            offset: start,
            line,
            column,
            snippet,
        });
    }
}

/// Convert a byte offset into a `(line, column)` pair, both 1-based, counting
/// characters (not bytes) for the column. Out-of-range `offset` clamps to the
/// text length.
fn offset_to_line_column(text: &str, offset: usize) -> (u32, u32) {
    let clipped = offset.min(text.len());
    let preceding = &text[..clipped];
    let line = preceding.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let line_start = preceding.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = text[line_start..clipped].chars().count() as u32 + 1;
    (line, column)
}

/// Extract the line containing `offset`, trim leading whitespace, and cap at
/// [`SNIPPET_MAX_CHARS`] for compact display.
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
    use super::{line_snippet, offset_to_line_column, scan_file, SearchMatch};
    use crate::{
        app::CODE_SEARCH_DEBOUNCE,
        host::{FakeFs, FsHost},
        test_harness::TestHarness,
    };
    use regex::Regex;
    use std::path::{Path, PathBuf};

    fn open_over(files: &[(&str, &str)]) -> TestHarness {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/repo");
        for (name, contents) in files {
            h.fake_fs()
                .insert_file(root.join(name), contents.as_bytes());
        }
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h
    }

    /// Type `query`, arm the debounce, then fire it so the streamed scan lands.
    fn run_query(h: &mut TestHarness, query: &str) {
        h.type_text(query);
        h.settle();
        h.advance_clock(CODE_SEARCH_DEBOUNCE);
    }

    #[test]
    fn typing_a_pattern_streams_matches() {
        let mut h = open_over(&[
            ("a.rs", "fn alpha() {}\nfn beta() {}\n"),
            ("b.rs", "fn alpha_again() {}\n"),
        ]);
        run_query(&mut h, "alpha");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let mut snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        snippets.sort();
        assert_eq!(snippets, ["fn alpha() {}", "fn alpha_again() {}"]);
    }

    #[test]
    fn selecting_a_match_scrolls_the_preview_to_its_line() {
        let content: String = (0..20)
            .map(|i| if i == 14 { "target here\n" } else { "filler\n" })
            .collect();
        let mut h = open_over(&[("a.rs", &content)]);
        run_query(&mut h, "target");

        let preview_editor = {
            let finder = h.stoat.code_search.as_ref().expect("code search open");
            assert_eq!(finder.matches.len(), 1, "one match on line 15");
            finder.preview.editor
        };
        let scroll_row = h
            .stoat
            .active_workspace()
            .editors
            .get(preview_editor)
            .expect("preview editor")
            .scroll_row;
        assert_eq!(
            scroll_row, 9,
            "the preview scrolls the match line a few rows down"
        );
    }

    #[test]
    fn enter_opens_the_file_at_the_match() {
        let mut h = open_over(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        run_query(&mut h, "beta");
        h.type_keys("enter");

        assert!(h.stoat.code_search.is_none(), "selecting closes the modal");
        let (buffer_id, offset) = h.stoat.focused_cursor_pos().expect("focused cursor");
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let row = buffer
            .read()
            .expect("poisoned")
            .rope()
            .offset_to_point(offset)
            .row;
        assert_eq!(row, 1, "the cursor lands on beta's line");
    }

    #[test]
    fn escape_disposes_the_scratch_buffers() {
        let mut h = crate::Stoat::test();
        let before = h.stoat.active_workspace().buffers.len();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        assert!(
            h.stoat.active_workspace().buffers.len() > before,
            "opening allocates the input and preview scratch buffers"
        );

        h.type_keys("escape");
        assert!(h.stoat.code_search.is_none(), "escape closes the modal");
        assert_eq!(
            h.stoat.active_workspace().buffers.len(),
            before,
            "closing disposes the scratch buffers"
        );
    }

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

    /// Non-streaming reference for [`scan_file`], scanning every workspace file
    /// in one pass so the per-file scan can be exercised over a whole tree.
    fn perform_search(
        fs_host: &dyn FsHost,
        git_root: &Path,
        pattern: &str,
    ) -> Result<Vec<SearchMatch>, regex::Error> {
        let regex = Regex::new(pattern)?;
        let mut matches = Vec::new();
        for path in fs_host.walk_workspace_files(git_root) {
            scan_file(fs_host, &regex, &path, &mut matches);
        }
        Ok(matches)
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
