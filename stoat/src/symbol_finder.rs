use crate::{
    buffer::BufferId,
    fuzzy,
    host::OffsetEncoding,
    input_view::{InputView, SubmitTarget},
    workspace::Workspace,
};
use lsp_types::{Position, SymbolKind};
use std::path::PathBuf;
use stoat_scheduler::Executor;

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
}

impl SymbolFinder {
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        buffer_id: BufferId,
        scope: SymbolFinderScope,
        servers: Vec<String>,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::SymbolFinder, "", "insert", 1);
        Self {
            input,
            scope,
            entries: Vec::new(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            buffer_id,
            servers,
            last_query: String::new(),
            query_dirty: false,
        }
    }

    /// Replace the symbol list and re-run the current `query` over it.
    pub(crate) fn set_entries(&mut self, entries: Vec<SymbolFinderEntry>, query: &str) {
        self.entries = entries;
        self.refilter(query);
    }

    /// Re-rank `entries` for `query`, matches first by score descending then
    /// title ascending. An empty or whitespace-only query lists every entry in
    /// document order with no highlights.
    pub(crate) fn refilter(&mut self, query: &str) {
        let (filtered, match_indices) = rank_entries(&self.entries, query);
        self.filtered = filtered;
        self.match_indices = match_indices;
        self.clamp_selected();
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        let step = self
            .viewport_rows
            .map(|v| v.div_ceil(2).max(1))
            .unwrap_or(1) as i32;
        self.move_selection(dir * step);
    }

    /// The entry under the selection cursor, or `None` for an empty list.
    pub(crate) fn selected_entry(&self) -> Option<&SymbolFinderEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
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
        .map(|(idx, entry)| (idx, entry.title.clone()));
    let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
        return (
            (0..entries.len()).collect(),
            vec![Vec::new(); entries.len()],
        );
    };
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.haystack.cmp(&b.haystack))
    });
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
    use super::{rank_entries, SymbolFinder, SymbolFinderEntry, SymbolFinderScope, SymbolTarget};
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        input_view::{InputView, SubmitTarget},
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
            buffer_id: BufferId::new(0),
            servers: Vec::new(),
            last_query: String::new(),
            query_dirty: false,
        };
        f.refilter("");
        f
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
