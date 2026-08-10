use crate::{
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    picker::Preview,
    workspace::Workspace,
};
use ast_grep_core::Pattern;
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use stoat_language::Language;
use stoat_scheduler::Executor;

pub(crate) mod ast;

/// Match sites the live scan streams before it stops, so a pattern matching most
/// of the workspace never overruns the list or the preview.
pub(crate) const MATCH_CAP: usize = 500;

/// How the code-search query is interpreted.
///
/// [`SearchMode::Regex`] is the default. [`SearchMode::Ast`] parses the query as
/// an ast-grep pattern and matches it against files of the finder's target
/// language, reached by Shift-Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Regex,
    Ast,
}

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
    pub(crate) mode: SearchMode,
    /// The language AST mode matches against, resolved from the focused buffer at
    /// open. `None` when that buffer has no language, which leaves AST mode
    /// unavailable and its toggle a no-op.
    pub(crate) target_lang: Option<Arc<Language>>,
    /// Set when the current AST query fails to parse, so the render shows a
    /// placeholder rather than a silently empty list.
    pub(crate) invalid_pattern: bool,
    /// Rows the match list rendered last, driving the page step. `None` until
    /// the first render lays the pane out.
    pub(crate) viewport_rows: Option<usize>,
    /// Parses the AST scan reuses while this modal is open, shared with the
    /// scan task. Held here so closing the finder drops a workspace's worth of
    /// syntax trees rather than leaving them for the process's lifetime.
    pub(crate) parse_cache: Arc<Mutex<ast::AstParseCache>>,
}

impl CodeSearchFinder {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        target_lang: Option<Arc<Language>>,
    ) -> Self {
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
            mode: SearchMode::Regex,
            target_lang,
            invalid_pattern: false,
            viewport_rows: None,
            parse_cache: Arc::new(Mutex::new(ast::AstParseCache::new(ast::PARSE_CACHE_CAP))),
        }
    }

    /// Whether `query` compiles under the current mode.
    ///
    /// An empty query is never valid (an empty regex would match every
    /// position). AST mode needs a resolved target language and a query that
    /// parses as an ast-grep pattern.
    pub(crate) fn pattern_valid(&self, query: &str) -> bool {
        if query.is_empty() {
            return false;
        }
        match self.mode {
            SearchMode::Regex => Regex::new(query).is_ok(),
            SearchMode::Ast => match &self.target_lang {
                Some(lang) => Pattern::try_new(query, ast::AstLang::new(lang.clone())).is_ok(),
                None => false,
            },
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
        crate::picker::nav_move(self.matches.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * crate::picker::nav_page_step(self.viewport_rows));
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.preview.dispose(ws);
    }
}

const SNIPPET_MAX_CHARS: usize = 80;

/// Read `path` through `fs_host`, scan its text for `regex`, and push one
/// [`SearchMatch`] per match site onto `out`.
///
/// `read_buf` is the caller's, so a walk hands the same one to every file it
/// visits rather than sizing a fresh buffer per file. Its contents afterwards
/// are the last file read and are not otherwise meaningful.
///
/// Anything [`read_text`] does not call text contributes nothing, so the scan
/// is total over an arbitrary workspace tree.
pub(crate) fn scan_file(
    fs_host: &dyn FsHost,
    regex: &Regex,
    path: &Path,
    read_buf: &mut Vec<u8>,
    out: &mut Vec<SearchMatch>,
) {
    let Some(text) = read_text(fs_host, path, read_buf) else {
        return;
    };
    scan_text(regex, text, path, out);
}

/// How much of a file's head is examined for the NUL byte that says it is not
/// text.
///
/// A binary essentially always has one in its header. Reading further to be
/// surer would cost the scan more than the rare miss does, and a file that
/// reaches this far without one is treated as text and left to UTF-8
/// validation.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Read `path` into `read_buf` and return it as text, or [`None`] when it is
/// not text at all.
///
/// `read_buf` is the caller's so a walk can reuse one allocation across the
/// files it visits rather than sizing a fresh one per file.
///
/// A NUL near the start ends it before UTF-8 validation, which is what keeps a
/// large binary from being validated end to end only to be thrown away. Files
/// that fail to read contribute nothing either, so a scan is total over an
/// arbitrary tree.
pub(crate) fn read_text<'a>(
    fs_host: &dyn FsHost,
    path: &Path,
    read_buf: &'a mut Vec<u8>,
) -> Option<&'a str> {
    fs_host.read(path, read_buf).ok()?;

    let head = &read_buf[..read_buf.len().min(BINARY_SNIFF_BYTES)];
    if head.contains(&0) {
        return None;
    }

    std::str::from_utf8(read_buf).ok()
}

/// Scan `text` for `regex`, pushing one [`SearchMatch`] per match site onto
/// `out`, each reporting `path` as where it was found.
///
/// The text is the caller's, which is what lets a search read an edited buffer
/// rather than the file behind it. Offsets are into `text`, so a caller
/// supplying buffer text gets offsets that index that buffer.
pub(crate) fn scan_text(regex: &Regex, text: &str, path: &Path, out: &mut Vec<SearchMatch>) {
    // Matches arrive in ascending order, so each one's position is the last
    // one's plus what lies between them. Recomputing from the start of the file
    // each time would walk the file once per match.
    let mut counted_to = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;

    for m in regex.find_iter(text).take(MATCH_CAP) {
        let start = m.start();

        let since = &text[counted_to..start];
        line += since.bytes().filter(|&b| b == b'\n').count() as u32;
        if let Some(last) = since.rfind('\n') {
            line_start = counted_to + last + 1;
        }
        counted_to = start;

        out.push(SearchMatch {
            path: path.to_path_buf(),
            offset: start,
            line,
            column: text[line_start..start].chars().count() as u32 + 1,
            snippet: line_snippet(text, line_start, start),
        });
    }
}

/// Convert a byte offset into a `(line, column)` pair, both 1-based, counting
/// characters (not bytes) for the column. Out-of-range `offset` clamps to the
/// text length.
///
/// Walks from the start of `text`, so a caller with several ascending offsets
/// carries its own state instead, as [`scan_file`] does.
pub(crate) fn offset_to_line_column(text: &str, offset: usize) -> (u32, u32) {
    let clipped = offset.min(text.len());
    let preceding = &text[..clipped];
    let line = preceding.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let column = text[line_start_at(text, clipped)..clipped].chars().count() as u32 + 1;
    (line, column)
}

/// Where the line containing `offset` begins, by searching back for the
/// newline before it.
///
/// For a caller holding one offset. One walking ascending offsets carries the
/// line start forward instead, as [`scan_file`] does.
pub(crate) fn line_start_at(text: &str, offset: usize) -> usize {
    let clipped = offset.min(text.len());
    text[..clipped].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Extract the line starting at `line_start` and containing `offset`, trim
/// leading whitespace, and cap at [`SNIPPET_MAX_CHARS`] for compact display.
///
/// The line start is passed rather than searched for, the caller already
/// knowing it from the position it computed.
fn line_snippet(text: &str, line_start: usize, offset: usize) -> String {
    let clipped = offset.min(text.len());
    let line_end = text[clipped..]
        .find('\n')
        .map(|i| clipped + i)
        .unwrap_or(text.len());
    let raw = &text[line_start..line_end];
    raw.trim_start().chars().take(SNIPPET_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        line_snippet, line_start_at, offset_to_line_column, scan_file, SearchMatch, SearchMode,
        MATCH_CAP,
    };
    use crate::{
        app::{CODE_SEARCH_AST_DEBOUNCE, CODE_SEARCH_DEBOUNCE},
        host::{FakeFs, FsHost},
        test_harness::TestHarness,
    };
    use regex::Regex;
    use std::path::{Path, PathBuf};

    /// Open the code-search modal with a `.rs` file focused, then Shift-Tab into
    /// AST mode. The focused rust buffer resolves rust as the target language.
    fn open_ast_over(files: &[(&str, &str)], focus: &str) -> TestHarness {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/repo");
        for (name, contents) in files {
            h.fake_fs()
                .insert_file(root.join(name), contents.as_bytes());
        }
        h.stoat.active_workspace_mut().git_root = root.clone();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join(focus),
            },
        );
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.type_keys("backtab");
        h
    }

    /// Type an AST `query` and fire the longer AST debounce so the scan lands.
    fn run_ast_query(h: &mut TestHarness, query: &str) {
        h.type_text(query);
        h.settle();
        h.advance_clock(CODE_SEARCH_AST_DEBOUNCE);
    }

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

    /// Open `name` under the root, type `insert` into it, and leave it dirty
    /// and unsaved, then open the code-search modal over the workspace.
    fn open_over_with_edit(files: &[(&str, &str)], name: &str, insert: &str) -> TestHarness {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/repo");
        for (file, contents) in files {
            h.fake_fs()
                .insert_file(root.join(file), contents.as_bytes());
        }
        h.stoat.active_workspace_mut().git_root = root.clone();

        h.open_file(&root.join(name));
        h.settle();
        h.type_keys("i");
        h.type_text(insert);
        h.type_keys("escape");
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h
    }

    /// A search reads the buffer the user is editing, not the file behind it.
    /// The word typed here exists nowhere on disk.
    #[test]
    fn an_unsaved_edit_is_searchable() {
        let mut h = open_over_with_edit(&[("a.rs", "fn saved() {}\n")], "a.rs", "// scribbled\n");
        run_query(&mut h, "scribbled");

        let finder = h.stoat.code_search.as_ref().expect("modal open");
        assert_eq!(
            finder.matches.len(),
            1,
            "the unsaved word matches: {:?}",
            finder.matches,
        );
        assert!(finder.matches[0].path.ends_with("a.rs"));
    }

    /// The overlay replaces the file rather than adding to it, so text present
    /// only on disk does not match while the buffer over it is edited.
    #[test]
    fn a_dirty_buffer_hides_what_only_disk_holds() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/repo");
        h.fake_fs()
            .insert_file(root.join("a.rs"), b"fn kept() {}\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        h.open_file(&root.join("a.rs"));
        h.settle();
        h.type_keys("i");
        h.type_text("// edited\n");
        h.type_keys("escape");
        h.settle();

        // The file moves on under the open buffer, which is what an external
        // write does. The buffer never saw this word, so a search that reads
        // the buffer must not find it.
        h.fake_fs()
            .insert_file(root.join("a.rs"), b"fn kept() {}\nfn disk_only() {}\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        run_query(&mut h, "disk_only");

        let finder = h.stoat.code_search.as_ref().expect("modal open");
        assert!(
            finder.matches.is_empty(),
            "text only on disk must not match under a dirty buffer: {:?}",
            finder.matches,
        );
    }

    /// A buffer the walk never offers still matches, which is what lets a file
    /// that has never been written show up at all.
    #[test]
    fn a_dirty_buffer_outside_the_walk_still_matches() {
        // The walk is rooted at /repo, so a buffer under /elsewhere is never
        // offered to it.
        let mut h = crate::Stoat::test();
        h.fake_fs()
            .insert_file(PathBuf::from("/elsewhere/b.rs"), b"fn other() {}\n");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

        h.open_file(Path::new("/elsewhere/b.rs"));
        h.settle();
        h.type_keys("i");
        h.type_text("// offsite\n");
        h.type_keys("escape");
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        run_query(&mut h, "offsite");

        let finder = h.stoat.code_search.as_ref().expect("modal open");
        assert_eq!(
            finder.matches.len(),
            1,
            "an unwalked dirty buffer still matches: {:?}",
            finder.matches,
        );
    }

    /// The offset a match reports has to index the text it was found in, since
    /// selecting it jumps the live buffer to that byte.
    #[test]
    fn a_match_offset_indexes_the_edited_buffer() {
        let inserted = "// pad pad pad\n";
        let mut h = open_over_with_edit(&[("a.rs", "fn target() {}\n")], "a.rs", inserted);
        run_query(&mut h, "target");

        let finder = h.stoat.code_search.as_ref().expect("modal open");
        assert_eq!(finder.matches.len(), 1, "one match: {:?}", finder.matches);
        let found = &finder.matches[0];

        let text = {
            let ws = h.stoat.active_workspace();
            let id = ws
                .buffers
                .id_for_path(Path::new("/repo/a.rs"))
                .expect("the buffer is open");
            let buffer = ws.buffers.get(id).expect("the buffer is open");
            let read = buffer.read().expect("buffer poisoned");
            read.snapshot.visible_text.to_string()
        };
        assert!(
            text[found.offset..].starts_with("target"),
            "offset {} must land on the match in the buffer, found {:?}",
            found.offset,
            &text[found.offset..(found.offset + 10).min(text.len())],
        );
        assert!(
            found.offset > inserted.len() - 2,
            "and must have moved past the inserted text, got {}",
            found.offset,
        );
    }

    /// Twelve matches over a viewport of six, so a page is three rows and both
    /// ends clamp within two pages.
    #[test]
    fn paging_steps_half_the_viewport_and_clamps() {
        let lines: String = (0..12).map(|i| format!("fn hit{i}() {{}}\n")).collect();
        let mut h = open_over(&[("a.rs", &lines)]);
        run_query(&mut h, "hit");

        let finder = h.stoat.code_search.as_mut().expect("modal open");
        assert_eq!(finder.matches.len(), 12);
        finder.viewport_rows = Some(6);

        finder.page(1);
        assert_eq!(finder.selected, 3, "a page down is half the viewport");
        finder.page(1);
        assert_eq!(finder.selected, 6);
        finder.page(-1);
        assert_eq!(finder.selected, 3, "a page up steps back by the same half");

        finder.page(1);
        finder.page(1);
        finder.page(1);
        assert_eq!(
            finder.selected, 11,
            "paging past the end clamps to the last"
        );
        finder.page(-1);
        finder.page(-1);
        finder.page(-1);
        finder.page(-1);
        finder.page(-1);
        assert_eq!(
            finder.selected, 0,
            "paging past the start clamps to the first"
        );
    }

    /// The viewport is only unset between opening the modal and its first
    /// render, a window the harness closes on its own, so it is cleared here to
    /// reach the fallback path.
    #[test]
    fn paging_with_an_unset_viewport_steps_one_row() {
        let lines: String = (0..4).map(|i| format!("fn hit{i}() {{}}\n")).collect();
        let mut h = open_over(&[("a.rs", &lines)]);
        run_query(&mut h, "hit");

        let finder = h.stoat.code_search.as_mut().expect("modal open");
        finder.viewport_rows = None;

        finder.page(1);
        assert_eq!(finder.selected, 1, "an unset viewport steps a single row");
    }

    /// The unit tests above call `page` directly and so cannot see the keymap.
    /// This covers the wiring instead. Ctrl-f has to reach the handler rather
    /// than doing nothing, which is the whole point of the item.
    #[test]
    fn ctrl_f_pages_the_code_search_selection() {
        let lines: String = (0..12).map(|i| format!("fn hit{i}() {{}}\n")).collect();
        let mut h = open_over(&[("a.rs", &lines)]);
        run_query(&mut h, "hit");
        let _ = h.snapshot();

        let before = h.stoat.code_search.as_ref().expect("modal open").selected;
        assert_eq!(before, 0);
        let viewport = h
            .stoat
            .code_search
            .as_ref()
            .expect("modal open")
            .viewport_rows
            .expect("the render stamped the viewport");

        h.type_keys("ctrl-f");
        assert_eq!(
            h.stoat.code_search.as_ref().expect("modal open").selected,
            viewport.div_ceil(2).max(1),
            "Ctrl-F pages down by half the rendered list height"
        );

        h.type_keys("ctrl-b");
        assert_eq!(
            h.stoat.code_search.as_ref().expect("modal open").selected,
            0,
            "Ctrl-B pages back up"
        );
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

    #[test]
    fn backtab_toggles_between_regex_and_ast() {
        let mut h = open_ast_over(&[("a.rs", "fn a() {}\n")], "a.rs");
        assert_eq!(
            h.stoat.code_search.as_ref().unwrap().mode,
            SearchMode::Ast,
            "open_ast_over lands in AST mode"
        );
        h.type_keys("backtab");
        assert_eq!(
            h.stoat.code_search.as_ref().unwrap().mode,
            SearchMode::Regex,
            "a second backtab returns to regex"
        );
    }

    #[test]
    fn ast_mode_matches_target_language_and_skips_others() {
        let mut h = open_ast_over(
            &[("a.rs", "fn alpha() {}\n"), ("b.txt", "fn alpha() {}\n")],
            "a.rs",
        );
        run_ast_query(&mut h, "fn $NAME() {}");

        let finder = h.stoat.code_search.as_ref().unwrap();
        assert_eq!(
            finder.matches.len(),
            1,
            "only the rust file matches, the .txt is skipped"
        );
        assert!(finder.matches[0].path.ends_with("a.rs"));
    }

    #[test]
    fn ast_invalid_pattern_flags_the_placeholder() {
        let mut h = open_ast_over(&[("a.rs", "fn a() {}\n")], "a.rs");
        // Two top-level items are not a single ast-grep pattern node.
        h.type_text("fn a() {} fn b() {}");
        h.settle();
        let _ = h.snapshot();

        let finder = h.stoat.code_search.as_ref().unwrap();
        assert!(
            finder.invalid_pattern,
            "an unparseable pattern flags invalid"
        );
        assert!(finder.matches.is_empty());
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
        let mut read_buf = Vec::new();
        for path in fs_host.walk_workspace_files(git_root) {
            scan_file(fs_host, &regex, &path, &mut read_buf, &mut matches);
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

    /// A file with matches spread across many lines, at varying distances and
    /// with multi-byte characters between them, which is the shape a carried
    /// position drifts on.
    fn dense_matches() -> String {
        let mut text = String::new();
        for row in 0..200 {
            match row % 4 {
                0 => text.push_str("needle at the start\n"),
                1 => text.push_str("    caf\u{e9} indented needle here\n"),
                2 => text.push_str("no match on this line at all\n"),
                _ => text.push_str("needle needle twice\n"),
            }
        }
        text
    }

    #[test]
    fn carrying_the_position_lands_where_recomputing_would() {
        let text = dense_matches();
        let (fs, root) = fake_with(&[("dense.rs", text.as_str())]);

        let regex = Regex::new("needle").expect("the pattern compiles");
        let mut found = Vec::new();
        scan_file(
            &fs,
            &regex,
            &root.join("dense.rs"),
            &mut Vec::new(),
            &mut found,
        );

        assert!(
            found.len() > 100,
            "the fixture has to carry far enough to drift: {}",
            found.len()
        );

        for m in &found {
            let (line, column) = offset_to_line_column(&text, m.offset);
            assert_eq!(
                (m.line, m.column),
                (line, column),
                "carried position differs at offset {}",
                m.offset
            );
            assert_eq!(
                m.snippet,
                line_snippet(&text, line_start_at(&text, m.offset), m.offset),
                "carried snippet differs at offset {}",
                m.offset
            );
        }
    }

    #[test]
    fn one_file_yields_no_more_than_the_cap() {
        let text = "needle\n".repeat(MATCH_CAP + 50);
        let (fs, root) = fake_with(&[("many.rs", text.as_str())]);

        let regex = Regex::new("needle").expect("the pattern compiles");
        let mut found = Vec::new();
        scan_file(
            &fs,
            &regex,
            &root.join("many.rs"),
            &mut Vec::new(),
            &mut found,
        );

        assert_eq!(
            found.len(),
            MATCH_CAP,
            "nothing downstream can show more, so the file stops there"
        );
        assert_eq!(found[0].line, 1, "and it stops at the end, not the start");
    }

    #[test]
    fn line_snippet_returns_full_line_trimmed() {
        assert_eq!(line_snippet("    hello\nbeta\n", 0, 4), "hello");
        assert_eq!(line_snippet("alpha\n  beta\n", 6, 8), "beta");
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

    /// A NUL in the sniffed head rejects the file before UTF-8 validation, and
    /// one past the head does not, which pins the window rather than only the
    /// rule. A file whose NUL sits beyond it is still rejected, just later, by
    /// the UTF-8 check that a NUL alone would pass.
    #[test]
    fn a_nul_in_the_head_rejects_the_file_before_validating_it() {
        use super::BINARY_SNIFF_BYTES;

        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");

        let mut early = b"needle ".to_vec();
        early.push(0);
        fs.insert_file(root.join("early.bin"), early);

        // Text right up to the window, then a NUL past it. Valid UTF-8
        // throughout, so only the sniff decides, and only where it looks.
        let mut late = vec![b'x'; BINARY_SNIFF_BYTES];
        late.extend_from_slice(b" needle ");
        late.push(0);
        fs.insert_file(root.join("late.bin"), late);

        let matches = perform_search(&fs, &root, "needle").unwrap();
        let found: Vec<_> = matches
            .iter()
            .map(|m| m.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            found,
            ["late.bin"],
            "only the file whose NUL sits past the sniff window is scanned",
        );
    }

    #[test]
    fn perform_search_empty_pattern_compiles_and_matches_every_position() {
        let (fs, root) = fake_with(&[("a.rs", "ab")]);
        let matches = perform_search(&fs, &root, "").unwrap();
        assert!(!matches.is_empty(), "empty regex should match");
    }
}
