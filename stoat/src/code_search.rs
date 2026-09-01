use crate::{
    host::FsHost,
    input_view::{InputView, SubmitTarget},
    picker::Preview,
    workspace::Workspace,
};
use ast_grep_core::Pattern;
use regex::Regex;
use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use stoat_host::fs::WALK_BATCH_SIZE;
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
    /// Shared rather than owned, because every match in a file names that same
    /// file and a dense one runs to [`MATCH_CAP`] of them.
    pub path: Arc<Path>,
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
    /// Rows the preview pane rendered last, driving where a match line sits
    /// in it. `None` until the first render lays the pane out.
    pub(crate) preview_rows: Option<usize>,
    /// Parses the AST scan reuses while this modal is open, shared with the
    /// scan task. Held here so closing the finder drops a workspace's worth of
    /// syntax trees rather than leaving them for the process's lifetime.
    ///
    /// Sharded because a parse is read through a `&mut` borrow of its cache, so
    /// one cache hands the whole scan back to a single thread.
    /// [`ast::parse_cache_shard`] routes a path, always to the same shard.
    pub(crate) parse_cache: Arc<[Mutex<ast::AstParseCache>; ast::PARSE_CACHE_SHARDS]>,
    /// Every file the workspace walk found, published by the first scan that
    /// completed one and read by every scan after it.
    ///
    /// Refining a query re-scans the same unchanged tree, and traversing it
    /// while matching gitignore rules per entry is the fixed cost that
    /// dominates on a repo of many small files. Shared with the scan task,
    /// which is what lets a blocking walk publish to a finder it cannot reach.
    ///
    /// Written once, so two scans racing to fill it settle on whichever
    /// finished first. A scan cut short by the match cap or by the user typing
    /// again leaves it empty rather than publishing the part of the tree it
    /// reached, which would silently shrink every later query.
    ///
    /// Seeded from [`crate::app::Stoat::finder_path_cache`] when that holds
    /// this root's tree, and published back into it once a walk completes, so
    /// the file finder and this modal walk the tree once between them.
    ///
    /// A file created or deleted while the modal is open stays invisible to
    /// every query after the first, since this is written once. Reopening the
    /// modal reads the cache again, which the watch events keep current.
    pub(crate) walked: Arc<OnceLock<Arc<Vec<PathBuf>>>>,
    /// The [`crate::app::Stoat::finder_path_epoch`] this modal opened under,
    /// which stamps the cache a completed walk publishes.
    ///
    /// Read at open rather than when a walk starts, because a walk starts at or
    /// after that moment. An event arriving during one leaves the stamp behind
    /// the epoch, and the next finder open walks rather than trusting a list
    /// the event already moved past.
    pub(crate) walk_epoch: u64,
}

impl CodeSearchFinder {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        target_lang: Option<Arc<Language>>,
        walked: Option<Arc<Vec<PathBuf>>>,
        walk_epoch: u64,
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
            preview_rows: None,
            parse_cache: Arc::new(std::array::from_fn(|_| {
                Mutex::new(ast::AstParseCache::new(ast::PARSE_CACHE_SHARD_CAP))
            })),
            walked: Arc::new(match walked {
                Some(paths) => OnceLock::from(paths),
                None => OnceLock::new(),
            }),
            walk_epoch,
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

/// Call `on_batch` with every path in `paths`, spreading the batches across the
/// machine's cores.
///
/// The counterpart to [`FsHost::walk_workspace_files_parallel`] for a scan that
/// already holds its paths. Such a scan has no walker threads to do its work
/// on, and would otherwise read and match every file on the one thread it was
/// spawned on.
///
/// Batches are [`WALK_BATCH_SIZE`] paths, matching the walk, so a modal streams
/// its matches at the same granularity whether or not it walked.
///
/// Returning [`ControlFlow::Break`] stops the remaining threads before their
/// next batch, though one already inside a batch finishes it.
pub(crate) fn scan_paths_parallel(
    paths: &[PathBuf],
    on_batch: &(dyn Fn(&[PathBuf]) -> ControlFlow<()> + Sync),
) {
    let batches = paths.len().div_ceil(WALK_BATCH_SIZE);
    if batches == 0 {
        return;
    }

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..threads.min(batches) {
            scope.spawn(|| loop {
                let start = next.fetch_add(WALK_BATCH_SIZE, Ordering::Relaxed);
                if start >= paths.len() {
                    return;
                }
                let end = (start + WALK_BATCH_SIZE).min(paths.len());

                if on_batch(&paths[start..end]).is_break() {
                    // Parking the cursor past the end is what stops the other
                    // threads. A caller that has stopped reading wants the rest
                    // of the list abandoned, not just this thread's share.
                    next.store(paths.len(), Ordering::Relaxed);
                    return;
                }
            });
        }
    });
}

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
    let path: Arc<Path> = Arc::from(path);

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
            path: path.clone(),
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
        ast::{self, AstLang},
        line_snippet, line_start_at, offset_to_line_column, scan_file, scan_paths_parallel,
        SearchMatch, SearchMode, MATCH_CAP, WALK_BATCH_SIZE,
    };
    use crate::{
        debounce::{CODE_SEARCH_AST_DEBOUNCE, CODE_SEARCH_DEBOUNCE},
        host::{FakeFs, FsHost},
        test_harness::TestHarness,
    };
    use ast_grep_core::Pattern;
    use regex::Regex;
    use std::{
        ops::ControlFlow,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    /// Open the code-search modal with a `.rs` file focused, leaving it in regex
    /// mode. The focused rust buffer resolves rust as the target language, so
    /// Shift-Tab into AST mode is available.
    fn open_over_focused(files: &[(&str, &str)], focus: &str) -> TestHarness {
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
        h
    }

    /// Open the code-search modal with a `.rs` file focused, then Shift-Tab into
    /// AST mode.
    fn open_ast_over(files: &[(&str, &str)], focus: &str) -> TestHarness {
        let mut h = open_over_focused(files, focus);
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

    /// The prompt's text, for asserting what a recall wrote into it.
    fn prompt_text(h: &TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        h.stoat
            .code_search
            .as_ref()
            .expect("the modal is open")
            .input
            .text(ws)
    }

    /// Alt-Up walks back through submitted queries, newest first, and Alt-Down
    /// walks toward the newest again.
    #[test]
    fn alt_up_and_alt_down_walk_the_submitted_queries() {
        let mut h = open_over(&[("a.rs", "alpha\nbeta\n")]);
        run_query(&mut h, "alpha");
        h.type_keys("enter");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        run_query(&mut h, "beta");
        h.type_keys("enter");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.type_keys("alt-up");
        assert_eq!(prompt_text(&h), "beta", "the newest query comes back first");
        h.type_keys("alt-up");
        assert_eq!(prompt_text(&h), "alpha", "and the one before it next");
        h.type_keys("alt-down");
        assert_eq!(prompt_text(&h), "beta", "Alt-Down walks toward the newest");
    }

    /// A recalled query re-arms the scan, so the recall lists matches rather
    /// than filling the prompt over an empty list.
    #[test]
    fn a_recalled_query_re_runs_the_scan() {
        let mut h = open_over(&[("a.rs", "alpha\nbeta\n")]);
        run_query(&mut h, "alpha");
        h.type_keys("enter");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        h.type_keys("alt-up");
        h.settle();
        h.advance_clock(CODE_SEARCH_DEBOUNCE);

        assert_eq!(prompt_text(&h), "alpha", "the recall filled the prompt");
        assert_eq!(
            h.stoat.code_search.as_ref().expect("open").matches.len(),
            1,
            "and the scan ran for it"
        );
    }

    /// The needle filters the walk, so typing narrows what Alt-Up reaches.
    #[test]
    fn the_typed_text_is_the_needle_the_walk_filters_on() {
        let mut h = open_over(&[("a.rs", "alpha\nbeta\n")]);
        run_query(&mut h, "alpha");
        h.type_keys("enter");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        run_query(&mut h, "beta");
        h.type_keys("enter");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
        run_query(&mut h, "al");
        h.type_keys("alt-up");

        assert_eq!(
            prompt_text(&h),
            "alpha",
            "the needle skips the newer query it does not match"
        );
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

    /// The workspace scan answers the smart-case rule the buffer search does,
    /// so an all-lowercase query reaches a hit of any case.
    #[test]
    fn a_lowercase_query_reaches_an_uppercase_hit() {
        let mut h = open_over(&[("a.rs", "fn Alpha() {}\n")]);
        run_query(&mut h, "alpha");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        assert_eq!(snippets, ["fn Alpha() {}"]);
    }

    /// Turning smart case off holds every query to the case it was typed in.
    #[test]
    fn a_case_sensitive_setting_holds_a_query_to_its_case() {
        let mut h = open_over(&[("a.rs", "fn Alpha() {}\n")]);
        h.stoat.settings.search_smart_case = Some(false);
        run_query(&mut h, "alpha");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        assert_eq!(snippets, [] as [&str; 0]);
    }

    /// The scan runs the regex over a file's whole text, so an anchor has to
    /// mean a line boundary or it would only ever reach the first line.
    #[test]
    fn an_anchor_matches_a_line_rather_than_the_file() {
        let mut h = open_over(&[("a.rs", "alpha\nbeta\n")]);
        run_query(&mut h, "^beta");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        assert_eq!(snippets, ["beta"]);
    }

    /// A session walks the tree once and scans what that walk found from then
    /// on, so a refined query has to still reach every file. A cache missing
    /// what the first query did not match would show up here as the broader
    /// second query finding less than the narrower first one.
    #[test]
    fn a_refined_query_still_reaches_every_file() {
        let mut h = open_over(&[
            ("a.rs", "fn alpha() {}\n"),
            ("b.rs", "fn alphabet() {}\n"),
            ("c.rs", "fn alphanumeric() {}\n"),
        ]);
        run_query(&mut h, "fn");
        run_query(&mut h, " alpha");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let mut snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        snippets.sort();
        assert_eq!(
            snippets,
            ["fn alpha() {}", "fn alphabet() {}", "fn alphanumeric() {}"]
        );
    }

    /// The one walk per session is what a file appearing afterwards makes
    /// visible. A modal lives for seconds and is reopened per search, so a
    /// query never seeing a file created under it is the accepted price of not
    /// re-walking the tree per keystroke.
    #[test]
    fn a_file_created_after_the_first_query_does_not_match_the_next() {
        let mut h = open_over(&[("a.rs", "fn alpha() {}\n")]);
        run_query(&mut h, "fn");

        h.fake_fs()
            .insert_file(PathBuf::from("/repo/b.rs"), b"fn alpha_again() {}\n");
        run_query(&mut h, " alpha");

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        let snippets: Vec<&str> = finder.matches.iter().map(|m| m.snippet.as_str()).collect();
        assert_eq!(
            snippets,
            ["fn alpha() {}"],
            "the second query scans the first walk, which predates b.rs"
        );
    }

    /// One cache serves both modes, since Shift-Tab switches mode inside a
    /// session and either mode walks the same root. The regex query here is what
    /// walks. The AST scan after it reads what that walk found, which is why the
    /// file written in between does not match.
    #[test]
    fn ast_mode_scans_the_walk_regex_mode_published() {
        let mut h = open_over_focused(&[("a.rs", "fn alpha() {}\n")], "a.rs");
        run_query(&mut h, "alpha");

        h.fake_fs()
            .insert_file(PathBuf::from("/repo/b.rs"), b"fn alpha() {}\n");
        h.type_keys("backtab");
        h.settle();
        h.advance_clock(CODE_SEARCH_AST_DEBOUNCE);

        let finder = h.stoat.code_search.as_ref().expect("code search open");
        assert_eq!(
            finder.matches.len(),
            1,
            "the AST scan reads the regex walk, which predates b.rs: {:?}",
            finder.matches,
        );
        assert!(finder.matches[0].path.ends_with("a.rs"));
    }

    #[test]
    fn scan_paths_parallel_delivers_every_path_once() {
        let paths: Vec<PathBuf> = (0..2000)
            .map(|i| PathBuf::from(format!("/repo/f{i}.rs")))
            .collect();
        let seen = Mutex::new(Vec::new());

        scan_paths_parallel(&paths, &|batch| {
            seen.lock().expect("seen poisoned").extend_from_slice(batch);
            ControlFlow::Continue(())
        });

        let mut seen = seen.into_inner().expect("seen poisoned");
        seen.sort();
        let mut expected = paths;
        expected.sort();
        assert_eq!(seen, expected, "every path delivered exactly once");
    }

    /// One break has to stop the threads that did not break, not only the one
    /// that did. Exactly one batch breaks here, so a run that abandons the rest
    /// of the list looks nothing like one where the other threads carry on and
    /// finish all of it.
    ///
    /// The threads still reading are stopped by their next claim, which they
    /// reach in the time between the breaking callback returning and its
    /// caller parking the cursor. Getting through the whole list in that window
    /// would take them thousands of times longer than they have.
    #[test]
    fn scan_paths_parallel_stops_on_break() {
        let paths: Vec<PathBuf> = (0..40_000)
            .map(|i| PathBuf::from(format!("/repo/f{i}.rs")))
            .collect();
        let batches = AtomicUsize::new(0);

        scan_paths_parallel(&paths, &|_| match batches.fetch_add(1, Ordering::Relaxed) {
            0 => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        });

        let batches = batches.into_inner();
        let total = paths.len().div_ceil(WALK_BATCH_SIZE);
        assert!(
            batches < total,
            "the break abandoned the rest of the list, rather than running all {total} batches"
        );
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
        // The match sits on line 14, and the preview puts it a third of the
        // way down its pane. No frame has measured the pane here, so the
        // third comes off Preview::ROWS_FALLBACK.
        assert_eq!(
            scroll_row,
            14 - (crate::picker::Preview::ROWS_FALLBACK / 3) as u32,
            "the preview scrolls the match line a third of the way down"
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

    /// The AST scan spreads the workspace over cores and over parse-cache
    /// shards, so what it finds must not depend on which thread reached a file
    /// or on how the paths hashed.
    ///
    /// The reference stays serial and unsharded, which is what makes it a
    /// reference rather than a second copy of the thing under test.
    #[test]
    fn a_fanned_ast_scan_finds_what_a_serial_one_does() {
        // Enough files to fill several shards and outrun one batch, and enough
        // shapes that a routing mistake drops a distinguishable match.
        let files: Vec<(String, String)> = (0..40)
            .map(|index| {
                (
                    format!("src/mod{index}/file{index}.rs"),
                    format!("fn f{index}() {{}}\nstruct S{index};\nfn g{index}() {{}}\n"),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();

        let mut h = open_ast_over(&borrowed, "src/mod0/file0.rs");
        run_ast_query(&mut h, "fn $NAME() {}");

        let lang = AstLang::new(
            h.stoat
                .language_registry
                .for_path(Path::new("a.rs"))
                .expect("rust language"),
        );
        let pattern = Pattern::try_new("fn $NAME() {}", lang.clone()).expect("pattern compiles");
        let mut cache = ast::AstParseCache::new(ast::PARSE_CACHE_CAP);
        let mut expected = Vec::new();
        for (name, text) in &files {
            ast::ast_scan_file(
                text,
                &lang,
                &pattern,
                &PathBuf::from("/repo").join(name),
                &mut cache,
                &mut expected,
            );
        }

        let key = |m: &SearchMatch| (m.path.to_path_buf(), m.offset);
        let mut found: Vec<_> = h
            .stoat
            .code_search
            .as_ref()
            .expect("code search open")
            .matches
            .iter()
            .map(key)
            .collect();
        let mut expected: Vec<_> = expected.iter().map(key).collect();
        found.sort();
        expected.sort();

        assert!(!expected.is_empty(), "the fixture matches something");
        assert_eq!(found, expected, "the fan finds the serial scan's matches");
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
