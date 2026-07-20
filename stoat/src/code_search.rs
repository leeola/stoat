use crate::{
    global_search::SearchMatch,
    input_view::{InputView, SubmitTarget},
    picker::Preview,
    workspace::Workspace,
};
use stoat_scheduler::Executor;

/// Match sites the live scan streams before it stops, so a pattern matching most
/// of the workspace never overruns the list or the preview.
pub(crate) const MATCH_CAP: usize = 500;

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

#[cfg(test)]
mod tests {
    use crate::{app::CODE_SEARCH_DEBOUNCE, test_harness::TestHarness};
    use std::path::PathBuf;

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
}
