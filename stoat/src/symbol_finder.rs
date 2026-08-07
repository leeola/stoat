use crate::{
    buffer::BufferId,
    fuzzy,
    host::OffsetEncoding,
    input_view::{InputView, SubmitTarget},
    markdown::StyledLine,
    picker::Preview,
    theme::Theme,
    workspace::Workspace,
};
use lsp_types::{Position, SymbolKind};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};

/// Whether the finder lists the focused buffer's document symbols or the whole
/// workspace's symbols.
///
/// Document scope filters a fixed list locally. Workspace scope re-issues the
/// server request as the query changes, so its results also come from the
/// server, not just a local filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolFinderScope {
    Document,
    Workspace,
}

/// Where selecting a [`SymbolFinderEntry`] takes the cursor.
///
/// Document-symbol entries carry an [`Self::Offset`] into the finder's source
/// buffer. Workspace entries carry the target file and LSP position, resolved
/// against that file's server encoding when opened.
#[derive(Debug, Clone)]
pub(crate) enum SymbolTarget {
    Offset(usize),
    Workspace {
        path: PathBuf,
        position: Position,
        encoding: OffsetEncoding,
    },
}

/// One row in the [`SymbolFinder`] list.
///
/// `title` is the fuzzy-matched, dotted-path symbol name. `kind` drives the dim
/// kind column and `line` the trailing `:line` suffix, both painted by the
/// renderer. `target` is where selection jumps.
#[derive(Debug, Clone)]
pub(crate) struct SymbolFinderEntry {
    pub(crate) title: String,
    pub(crate) kind: Option<SymbolKind>,
    pub(crate) line: u32,
    pub(crate) target: SymbolTarget,
}

/// Centered finder modal over a buffer's document symbols.
///
/// Holds the flattened symbol list and a fuzzy view of it. `filtered` indexes
/// `entries` in display order and `match_indices` carries the matched character
/// offsets per filtered row for highlighting. An empty query lists every symbol
/// in document order, since symbol order is meaningful (unlike a path list).
pub(crate) struct SymbolFinder {
    pub(crate) input: InputView,
    pub(crate) scope: SymbolFinderScope,
    pub(crate) entries: Vec<SymbolFinderEntry>,
    pub(crate) filtered: Vec<usize>,
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    pub(crate) viewport_rows: Option<usize>,
    /// Read-only pane showing the selected symbol's source. Kept in sync by
    /// [`crate::action_handlers::lsp::sync_symbol_finder`].
    pub(crate) preview: Preview,
    /// Rows the preview pane rendered last, driving the scroll centering.
    /// `None` until the first render lays the pane out.
    pub(crate) preview_rows: Option<usize>,
    /// Buffer the finder opened over. The workspace scope routes re-issued
    /// requests through it when its named servers no longer resolve.
    pub(crate) buffer_id: BufferId,
    /// Workspace-symbol servers routed at open. Empty for document scope.
    pub(crate) servers: Vec<String>,
    /// Query a workspace re-issue last fired for, so a changed input triggers a
    /// fresh request. Unused for document scope, which filters locally.
    pub(crate) last_query: String,
    /// A query changed while a workspace request was in flight, so the pump
    /// re-fires with the current text once the in-flight request lands.
    pub(crate) query_dirty: bool,
    /// In-flight `textDocument/hover` for the selected symbol's documentation.
    pub(crate) pending_doc: Option<Task<Option<String>>>,
    /// Filtered index the pending or resolved doc corresponds to, so a stale
    /// response landing after the selection moved is discarded. `None` when no
    /// entry is selected.
    pub(crate) doc_for: Option<usize>,
    /// Resolved hover markdown for the selected symbol, rendered above the source
    /// preview. `None` when unresolved, empty, or the request failed.
    pub(crate) doc_markdown: Option<String>,
    /// [`Self::doc_markdown`] rendered, and the theme epoch it was styled under.
    ///
    /// Rendering it parses every fenced code block and builds a style per byte,
    /// and the renderer would otherwise do that on every paint the modal is up
    /// for a document that cannot have changed. Cleared wherever the markdown
    /// is written, and rebuilt by [`Self::styled_doc_lines`] when the theme
    /// moves under it.
    pub(crate) doc_lines: Option<(u64, Vec<StyledLine>)>,
    /// Rows the modal's list would need for the whole symbol set, which the
    /// renderer sizes the box against.
    ///
    /// Recorded only from an unfiltered result set, never from a narrowed one.
    /// Document scope filters [`Self::entries`] locally, but workspace scope
    /// re-issues the server request per keystroke and replaces them with
    /// query-specific hits, so reading the live count would resize the box under
    /// the user as they type.
    pub(crate) content_rows: u16,
    /// Bumped wherever [`Self::set_entries`] replaces the symbol list, so a
    /// ranking memoized against the old list is never reused for the new one.
    entries_generation: u64,
    /// Query and entry-list generation the current ranking came from.
    ///
    /// The ranking is a pure function of those two, so an unchanged key means
    /// an identical outcome. Ranking runs from the per-frame sync and walks
    /// every entry with a fuzzy traceback, so a modal left sitting over a
    /// workspace's symbols would otherwise re-rank thousands of them a frame.
    last_filter_key: Option<u64>,
    /// Ranking, selection, and pane height the preview pane was last built for.
    ///
    /// The ranking key belongs in it because a new query repoints the same
    /// selection index at a different symbol, and the pane height because the
    /// scroll puts the symbol a third of the way down a pane a resize moves.
    preview_for: Option<(Option<u64>, usize, usize)>,
}

impl SymbolFinder {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        buffer_id: BufferId,
        scope: SymbolFinderScope,
        servers: Vec<String>,
    ) -> Self {
        let preview = Preview::new(ws, executor.clone());
        let input = InputView::create(ws, executor, SubmitTarget::SymbolFinder, "", "insert", 1);
        Self {
            input,
            scope,
            entries: Vec::new(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview,
            preview_rows: None,
            buffer_id,
            servers,
            last_query: String::new(),
            query_dirty: false,
            pending_doc: None,
            doc_for: None,
            doc_markdown: None,
            doc_lines: None,
            content_rows: 0,
            entries_generation: 0,
            last_filter_key: None,
            preview_for: None,
        }
    }

    /// Replace the symbol list and re-run the current `query` over it.
    pub(crate) fn set_entries(&mut self, entries: Vec<SymbolFinderEntry>, query: &str) {
        if query.is_empty() {
            self.content_rows = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        }
        self.entries = entries;
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.refilter(query);
    }

    /// Re-rank `entries` for `query`, matches first by score descending then
    /// title ascending. An empty or whitespace-only query lists every entry in
    /// document order with no highlights.
    ///
    /// Re-ranking the same list for the same query is skipped, so the per-frame
    /// sync a modal drives costs nothing while the query sits still.
    pub(crate) fn refilter(&mut self, query: &str) {
        let key = {
            let mut hasher = DefaultHasher::new();
            query.hash(&mut hasher);
            self.entries_generation.hash(&mut hasher);
            hasher.finish()
        };
        if self.last_filter_key == Some(key) {
            return;
        }
        self.last_filter_key = Some(key);

        let (filtered, match_indices) = rank_entries(&self.entries, query);
        self.filtered = filtered;
        self.match_indices = match_indices;
        self.clamp_selected();
    }

    /// Whether the preview pane has to be rebuilt for a pane `rows` tall, and
    /// record that it was.
    ///
    /// True once per ranking, selection, and pane height, so a modal sitting
    /// still reloads and re-scrolls the pane once rather than every frame the
    /// run loop drives.
    pub(crate) fn preview_needs_sync(&mut self, rows: usize) -> bool {
        let key = (self.last_filter_key, self.selected, rows);
        if self.preview_for == Some(key) {
            return false;
        }
        self.preview_for = Some(key);
        true
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        crate::picker::nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * crate::picker::nav_page_step(self.viewport_rows));
    }

    /// The entry under the selection cursor, or `None` for an empty list.
    pub(crate) fn selected_entry(&self) -> Option<&SymbolFinderEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    /// The selected symbol's documentation as styled lines, or `None` when it
    /// has none.
    ///
    /// Renders [`Self::doc_markdown`] only when nothing is held for
    /// `theme_epoch`, so a modal painting frame after frame over one symbol
    /// renders it once. The styles are baked into the lines, which is why a
    /// theme change rebuilds them rather than reusing what it finds.
    pub(crate) fn styled_doc_lines(
        &mut self,
        theme_epoch: u64,
        theme: &Theme,
        languages: &LanguageRegistry,
    ) -> Option<&[StyledLine]> {
        let markdown = self.doc_markdown.as_deref()?;

        let held = self
            .doc_lines
            .as_ref()
            .is_some_and(|(epoch, _)| *epoch == theme_epoch);
        if !held {
            let lines = crate::markdown::render_markdown(markdown, theme, languages);
            self.doc_lines = Some((theme_epoch, lines));
        }

        self.doc_lines.as_ref().map(|(_, lines)| lines.as_slice())
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
        self.preview.dispose(ws);
    }

    fn clamp_selected(&mut self) {
        crate::picker::nav_clamp(self.filtered.len(), &mut self.selected);
    }
}

/// Rank `entries` for `query`, returning parallel `(filtered, match_indices)`
/// vectors. `filtered` holds indices into `entries` and `match_indices` the
/// matched character offsets in each row's title. Empty query yields document
/// order and no highlights.
fn rank_entries(entries: &[SymbolFinderEntry], query: &str) -> (Vec<usize>, Vec<Vec<u32>>) {
    let items = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| (idx, entry.title.as_str()));
    let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
        return (
            (0..entries.len()).collect(),
            vec![Vec::new(); entries.len()],
        );
    };
    fuzzy::sort_ranked(&mut matches);
    let mut filtered = Vec::with_capacity(matches.len());
    let mut match_indices = Vec::with_capacity(matches.len());
    for m in matches {
        filtered.push(m.item);
        match_indices.push(m.matched_indices);
    }
    (filtered, match_indices)
}

#[cfg(test)]
mod tests {
    use super::{
        rank_entries, StyledLine, SymbolFinder, SymbolFinderEntry, SymbolFinderScope, SymbolTarget,
    };
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        input_view::{InputView, SubmitTarget},
        picker::Preview,
    };

    fn entry(title: &str) -> SymbolFinderEntry {
        SymbolFinderEntry {
            title: title.to_string(),
            kind: None,
            line: 0,
            target: SymbolTarget::Offset(0),
        }
    }

    fn finder(titles: &[&str]) -> SymbolFinder {
        let input = InputView {
            editor_id: EditorId::default(),
            buffer_id: BufferId::new(0),
            target: SubmitTarget::SymbolFinder,
            max_height: 1,
        };
        let mut f = SymbolFinder {
            input,
            scope: SymbolFinderScope::Document,
            entries: titles.iter().map(|t| entry(t)).collect(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview: Preview::test_dummy(),
            preview_rows: None,
            buffer_id: BufferId::new(0),
            servers: Vec::new(),
            last_query: String::new(),
            query_dirty: false,
            pending_doc: None,
            doc_for: None,
            doc_markdown: None,
            doc_lines: None,
            content_rows: titles.len() as u16,
            entries_generation: 0,
            last_filter_key: None,
            preview_for: None,
        };
        f.refilter("");
        f
    }

    /// The renderer asks per frame while the modal is up, and rendering the
    /// markdown parses every fenced code block in it, so a second ask over the
    /// same document must not render again.
    ///
    /// The held lines are overwritten between the asks, which a re-render would
    /// replace and a reuse hands straight back.
    #[test]
    fn doc_lines_render_once_until_the_theme_moves() {
        use ratatui::style::Style;
        use stoat_language::LanguageRegistry;

        let mut f = finder(&["alpha"]);
        f.doc_markdown = Some("# heading\n\n```rust\nfn a() {}\n```".to_string());
        let theme = crate::theme::Theme::empty();
        let languages = LanguageRegistry::standard();

        assert!(
            f.styled_doc_lines(3, &theme, &languages).is_some(),
            "the first ask renders the markdown"
        );

        let held: Vec<StyledLine> = vec![vec![("held".to_string(), Style::default())]];
        f.doc_lines = Some((3, held.clone()));
        assert_eq!(
            f.styled_doc_lines(3, &theme, &languages),
            Some(held.as_slice()),
            "the same theme reuses what is held rather than rendering again"
        );

        assert_ne!(
            f.styled_doc_lines(4, &theme, &languages),
            Some(held.as_slice()),
            "a new theme restyles every span, so the held lines no longer describe it"
        );
    }

    #[test]
    fn empty_query_lists_in_document_order() {
        let entries: Vec<_> = ["zeta", "alpha", "mu"].iter().map(|t| entry(t)).collect();
        let (filtered, indices) = rank_entries(&entries, "");
        assert_eq!(filtered, vec![0, 1, 2]);
        assert_eq!(indices, vec![Vec::<u32>::new(); 3]);
    }

    #[test]
    fn query_ranks_matches_by_score_then_title() {
        let entries: Vec<_> = ["format_all", "fmt", "unrelated"]
            .iter()
            .map(|t| entry(t))
            .collect();
        let (filtered, indices) = rank_entries(&entries, "fmt");
        assert_eq!(
            filtered
                .iter()
                .map(|&i| entries[i].title.as_str())
                .collect::<Vec<_>>(),
            vec!["fmt", "format_all"],
            "both fuzzy-match fmt, the unrelated symbol is dropped"
        );
        assert!(
            !indices[0].is_empty(),
            "the top match carries highlight offsets"
        );
    }

    #[test]
    fn an_unchanged_query_reuses_the_ranking() {
        let mut f = finder(&["alpha", "beta"]);
        f.filtered.clear();

        f.refilter("");
        assert!(f.filtered.is_empty(), "the same query ranks nothing again");

        f.refilter("alph");
        assert_eq!(f.filtered, vec![0], "a changed query ranks afresh");
    }

    #[test]
    fn replacing_the_entries_ranks_the_same_query_again() {
        let mut f = finder(&["alpha"]);
        f.set_entries(vec![entry("beta"), entry("gamma")], "");
        assert_eq!(
            f.filtered,
            vec![0, 1],
            "a new list outranks a memo held for the old one"
        );
    }

    #[test]
    fn the_preview_rebuilds_once_per_ranking_selection_and_pane_height() {
        let mut f = finder(&["alpha", "alpine"]);
        assert!(f.preview_needs_sync(20), "the first sync builds the pane");
        assert!(!f.preview_needs_sync(20), "a still modal rebuilds nothing");

        f.move_selection(1);
        assert!(
            f.preview_needs_sync(20),
            "a moved selection is a new symbol"
        );
        assert!(f.preview_needs_sync(24), "a resized pane re-centers it");

        f.refilter("al");
        assert_eq!(f.selected, 1, "both entries still match, so it holds");
        assert!(
            f.preview_needs_sync(24),
            "a new ranking points the held index at a different symbol"
        );
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut f = finder(&["a", "b", "c"]);
        f.move_selection(-1);
        assert_eq!(f.selected, 0);
        f.move_selection(5);
        assert_eq!(f.selected, 2);
        f.move_selection(-1);
        assert_eq!(f.selected, 1);
    }

    #[test]
    fn page_steps_by_half_viewport() {
        let mut f = finder(&["a", "b", "c", "d", "e", "f"]);
        f.viewport_rows = Some(4);
        f.page(1);
        assert_eq!(f.selected, 2, "half of a 4-row viewport is 2");
        f.page(-1);
        assert_eq!(f.selected, 0);
    }

    #[test]
    fn refilter_clamps_stale_selection() {
        let mut f = finder(&["apple", "apricot", "banana"]);
        f.selected = 2;
        f.refilter("ap");
        assert_eq!(f.filtered.len(), 2);
        assert_eq!(f.selected, 1, "selection past the shorter list clamps");
    }

    #[test]
    fn selected_entry_follows_the_cursor() {
        let mut f = finder(&["a", "b", "c"]);
        f.move_selection(1);
        assert_eq!(f.selected_entry().map(|e| e.title.as_str()), Some("b"));
    }
}
