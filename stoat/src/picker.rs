use crate::{
    buffer::BufferId,
    editor_state::{EditorId, EditorState, ScrollGlide},
    fuzzy,
    host::FsHost,
    paths,
    render::sanitize,
    workspace::Workspace,
};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use stoat_language::LanguageRegistry;
use stoat_scheduler::{Executor, Task};
use stoat_text::{Bias, SelectionGoal};
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver, UnboundedSender};

/// Preview content cap. Keeps preview reads bounded so a stray large or binary
/// file never stalls the render thread.
pub(crate) const PREVIEW_BYTE_LIMIT: usize = 128 * 1024;

/// How far down a filtered list matched indices are derived eagerly.
///
/// Deriving them is the expensive half of matching, and a list this deep is
/// already far past what a viewport shows or a reader pages through. Rows below
/// it derive theirs when something actually paints them.
const INDEXED_ROWS: usize = 512;

/// Source of process-unique content-version stamps, shared by every pool that
/// versions its content by a monotonic generation instead of a content hash.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Next process-unique generation, bumped past every prior stamp.
pub(crate) fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Step a list cursor by `delta`, saturating at both ends of a `len`-long list.
///
/// Every modal list moves its cursor this way. The cursor is passed in rather
/// than reached for through a trait, because the pickers keep it in unrelated
/// places -- some beside a plain `Vec`, some inside a struct that gates writes
/// to it -- and a caller that has both numbers can always call this.
///
/// An empty list parks the cursor at 0, which is where a list that later gains
/// entries expects to find it.
pub(crate) fn nav_move(len: usize, selected: &mut usize, delta: i32) {
    if len == 0 {
        *selected = 0;
        return;
    }
    let max = (len - 1) as i32;
    *selected = (*selected as i32 + delta).clamp(0, max) as usize;
}

/// Rows a page key should move the cursor, given the list's rendered height.
///
/// Half a viewport keeps some of the previous screen in view, so the reader
/// keeps their place across the jump. `viewport_rows` is `None` until the first
/// render measures the list, and a page before then moves a single row rather
/// than guessing at a height.
pub(crate) fn nav_page_step(viewport_rows: Option<usize>) -> i32 {
    viewport_rows
        .map(|rows| rows.div_ceil(2).max(1))
        .unwrap_or(1) as i32
}

/// Pull a cursor back inside a `len`-long list that may have shrunk under it.
///
/// Filtering is what makes this necessary. A cursor resting on row 40 is
/// meaningless the moment a keystroke narrows the list to 3, so every refilter
/// ends here.
pub(crate) fn nav_clamp(len: usize, selected: &mut usize) {
    if len == 0 {
        *selected = 0;
    } else if *selected >= len {
        *selected = len - 1;
    }
}

/// Query-driven fuzzy result list over a fixed `base` set of paths, decoupled
/// from any input widget.
///
/// The file finder and the palette's inline pickers drive the same list from a
/// query string. The owner sets `base`, calls [`PickList::refilter`] with the
/// query, and reads `filtered`/`match_indices`/`selected` to render.
pub(crate) struct PickList {
    /// Candidate paths the query filters over.
    ///
    /// Owned rather than shared so a streaming walk can append to it, which is
    /// what lets the display strings and the current match survive a batch.
    /// Move it through [`Self::set_base`] or [`Self::extend_base`]. Replacing it
    /// by hand leaves [`Self::base_generation`] behind, and the display cache
    /// would then read stale rows as a prefix of the new set.
    pub(crate) base: Vec<PathBuf>,
    /// Bumped whenever [`Self::base`] is replaced, and deliberately not when it
    /// is extended.
    ///
    /// This is what makes a shorter display cache safe to treat as a prefix of
    /// a longer base rather than as something built from other paths entirely.
    pub(crate) base_generation: u64,
    /// Indices into `base`, after filtering, in display order.
    pub(crate) filtered: Vec<usize>,
    /// Per-row matched character offsets into the row's display string, for the
    /// leading [`Self::indexed`] rows of `filtered`. A row is empty when no
    /// pattern is active. The offsets are sorted and deduplicated so the
    /// renderer can `contains`-test without further work.
    ///
    /// Read it through [`Self::row_indices`], which covers the rows past the
    /// end of this.
    pub(crate) match_indices: Vec<Vec<u32>>,
    /// How many leading rows of `filtered` have their offsets in
    /// [`Self::match_indices`].
    ///
    /// Deriving offsets costs more than deciding what matched, so a list far
    /// longer than any viewport derives them only as deep as something can
    /// plausibly paint. Rows below derive theirs on demand.
    pub(crate) indexed: usize,
    pub(crate) selected: usize,
    /// Rendered list height in rows, refreshed each frame by the owner's render
    /// so [`PickList::page`] can size its half-page step. `None` before the
    /// first render, where the step falls back to a single row.
    pub(crate) viewport_rows: Option<usize>,
    /// Process-monotonic stamp bumped on construction and every refilter, so a
    /// render pool versions the filter state in O(1) instead of hashing the
    /// whole filtered index.
    pub(crate) filter_generation: u64,
    /// When set, rows and fuzzy haystacks render as `<owning-root basename>/<path
    /// relative to that root>` by longest-prefix match instead of relative to a
    /// single `git_root`. Set for the cross-workspace scope. `None` leaves every
    /// existing finder git-root-relative.
    pub(crate) display_roots: Option<Vec<PathBuf>>,
    /// Rendered display string per base path, or `None` before the first
    /// [`Self::refilter`].
    ///
    /// Derived state. [`Self::refilter`] rebuilds it whenever its inputs move,
    /// so clearing it is safe and setting it is not possible from outside this
    /// module.
    pub(crate) display: Option<DisplayCache>,
    /// The query [`Self::filtered`] holds the matches for, and how much of the
    /// base it had seen at the time.
    ///
    /// A longer query starts from those rows rather than the whole base, and a
    /// base that has since grown contributes only its tail, so the two compose
    /// into one candidate set.
    ///
    /// `None` wherever the rows stopped meaning anything. That is before the
    /// first refilter, once the base was replaced under them, and once a caller
    /// dropped the results outright.
    ///
    /// Derived state, like [`Self::display`]. [`Self::refilter`] maintains it
    /// against the rows it produces, and [`Self::clear_results`] is how an
    /// owner drops both together.
    pub(crate) last_filter: Option<(String, usize)>,
    /// How many candidates the last [`Self::refilter`] handed the matcher.
    ///
    /// A query that extends its predecessor scores only the rows that already
    /// matched, so this falls away from the base length as a query grows. It is
    /// the only outward sign that the narrowing happened, results being
    /// identical either way.
    pub(crate) scored: usize,
}

/// One query's scan over a pick list's display rows, carrying everything it
/// needs so it can run away from the list it came from.
///
/// [`PickList::begin_refilter`] produces one, [`Self::run`] does the matching,
/// and [`PickList::apply_scan`] takes the result. A caller wanting the scan off
/// its own thread puts `run` on a worker. One that does not simply calls it.
pub(crate) struct Scan {
    /// The whole query, kept so the result can say what it answers.
    query: String,
    /// The query with its `./` anchor removed, which is what actually matches.
    pattern: String,
    anchor: Option<String>,
    anchor_len: u32,
    rows: Arc<Vec<Arc<str>>>,
    candidates: Candidates,
    /// How much of the base the rows covered when this was prepared.
    covered: usize,
}

/// Which rows a [`Scan`] offers to the matcher.
enum Candidates {
    /// Every row, for a query that cannot narrow a previous result.
    Every,
    /// The rows a shorter query already matched, plus whatever a walk appended
    /// since it ran. Everything else was offered to that shorter query and
    /// rejected, and a longer one cannot revive it.
    Narrowed {
        previous: Vec<usize>,
        arrived: Range<usize>,
    },
}

/// What a [`Scan`] produced, in the shape [`PickList`] keeps it.
pub(crate) struct ScanOutcome {
    query: String,
    covered: usize,
    filtered: Vec<usize>,
    match_indices: Vec<Vec<u32>>,
    indexed: usize,
    scored: usize,
}

impl Scan {
    /// Match and rank, which is the expensive half and touches nothing but this.
    pub(crate) fn run(self) -> ScanOutcome {
        let anchor = self.anchor.as_deref();
        let keeps = |display: &str| anchor.is_none_or(|a| display.starts_with(a));

        let (ranked, scored) = match &self.candidates {
            Candidates::Every => {
                let items = self
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, display)| keeps(display))
                    .map(|(idx, display)| (idx, &**display));

                (
                    fuzzy::rank_indexing_best(&self.pattern, items, INDEXED_ROWS),
                    self.rows.len(),
                )
            },
            Candidates::Narrowed { previous, arrived } => {
                let items = previous
                    .iter()
                    .copied()
                    .chain(arrived.clone())
                    .filter(|&idx| keeps(&self.rows[idx]))
                    .map(|idx| (idx, &*self.rows[idx]));

                (
                    fuzzy::rank_indexing_best(&self.pattern, items, INDEXED_ROWS),
                    previous.len() + arrived.len(),
                )
            },
        };

        let mut outcome = ScanOutcome {
            query: self.query,
            covered: self.covered,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            indexed: 0,
            scored,
        };

        // `begin_refilter` only builds a scan for a query with atoms, so the
        // matcher always has something to rank.
        let Some(ranked) = ranked else {
            return outcome;
        };

        outcome.indexed = ranked.indexed;
        for (row, m) in ranked.matches.into_iter().enumerate() {
            outcome.filtered.push(m.item);
            if row < ranked.indexed {
                outcome
                    .match_indices
                    .push(prepend_anchor(self.anchor_len, m.matched_indices));
            }
        }

        outcome
    }
}

/// The display string of every base path, and the order an unfiltered list
/// shows them in.
///
/// Deriving one relativizes the path, substitutes the home directory, and
/// builds a `String`. The filter needs all of them on every keystroke while the
/// renderer formats only the rows on screen, so a large repo would spend most
/// of a keystroke rebuilding strings that did not change.
pub(crate) struct DisplayCache {
    /// Which candidate set these rows describe. Everything else here is what
    /// [`row_display`] reads, so a cache hit is equivalent to deriving the
    /// strings again.
    base_generation: u64,
    git_root: PathBuf,
    display_roots: Option<Vec<PathBuf>>,
    home: Option<PathBuf>,
    /// One string per base entry, in the same order, covering a prefix of the
    /// base. A walk that appended since this was built leaves the tail
    /// uncovered, and its length is what says where that tail starts.
    ///
    /// Shared rather than owned so a background scan can read the haystacks
    /// without a copy of them. Each row is itself shared, so the copy
    /// [`Arc::make_mut`] makes when a scan is in flight moves pointers rather
    /// than the strings, and appending a walk batch stays proportional to the
    /// batch.
    rows: Arc<Vec<Arc<str>>>,
    /// Indices into [`Self::rows`], ordered by the string each names, which is
    /// the order an empty query lists them in.
    sorted: Vec<usize>,
    /// Bumped on each rebuild, so a test can tell a reuse from a rebuild.
    pub(crate) generation: u64,
}

/// Which caller-owned candidate set a [`PathPicker::refilter_with_base`] is over.
///
/// `identity` names the set. Two calls carrying the same one are over a base
/// that has only been appended to since, which is what lets the rows already
/// derived for it stand. Anything that can reorder or replace entries has to
/// change it.
///
/// `len` is how long the set was, so the append is a tail to derive rather than
/// a list to rebuild.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BaseId {
    pub(crate) identity: u64,
    pub(crate) len: usize,
}

impl Default for PickList {
    fn default() -> Self {
        Self {
            base: Vec::new(),
            base_generation: next_generation(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            indexed: 0,
            selected: 0,
            viewport_rows: None,
            filter_generation: next_generation(),
            display_roots: None,
            display: None,
            last_filter: None,
            scored: 0,
        }
    }
}

impl PickList {
    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.filtered
            .get(self.selected)
            .and_then(|i| self.base.get(*i))
            .map(|p| p.as_path())
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Before the first render sets [`Self::viewport_rows`]
    /// the step falls back to a single row.
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * nav_page_step(self.viewport_rows));
    }

    /// Re-run the matcher over `base` for `query` via
    /// [`crate::fuzzy::match_and_rank`], ordering matches by score descending,
    /// ties alphabetical. Empty or whitespace-only input lists every candidate
    /// alphabetically.
    ///
    /// A leading `./` token anchors to the workspace root. Candidates are first
    /// restricted to those whose display path starts with the token's prefix,
    /// and the rest of the query fuzzy-matches within them. The anchored prefix
    /// is highlighted on every surviving row. See [`split_root_anchor`].
    ///
    /// `match_indices` is rebuilt in parallel to `filtered`. Each element is the
    /// sorted, deduplicated set of matched character offsets in that row's
    /// display string, or empty when no pattern or anchor is active.
    pub(crate) fn refilter(&mut self, query: &str, git_root: &Path) {
        if let Some(scan) = self.begin_refilter(query, git_root) {
            let outcome = scan.run();
            self.apply_scan(outcome);
        }
    }

    /// Refilter as far as this thread has to, handing back the scan when one is
    /// left to run.
    ///
    /// Everything needing the pick list itself happens here. The display strings
    /// are brought up to date against the base, and a query with no pattern is
    /// listed outright, that being a walk of an order already built rather than
    /// a scan. A query with a pattern yields a [`Scan`], which carries what it
    /// needs and so can run anywhere, and whose result goes to
    /// [`Self::apply_scan`].
    ///
    /// The results on display are left alone until then, so a caller running the
    /// scan elsewhere keeps painting the previous query's rows rather than
    /// blanking the list while it waits.
    pub(crate) fn begin_refilter(&mut self, query: &str, git_root: &Path) -> Option<Scan> {
        let (anchor, pattern) = split_root_anchor(query);
        let anchor_len = anchor.map_or(0, |a| a.chars().count()) as u32;

        // Before the rows are read, since a rebuild here is what makes the
        // previous result meaningless.
        self.ensure_display(git_root);

        // A grown base contributes only what arrived after the previous result,
        // every row before that having already been offered to the same query.
        let arrived = match self.narrows_previous(query, anchor, pattern) {
            true => self.last_filter.as_ref().map(|&(_, covered)| covered),
            false => None,
        };

        let cache = self.display.as_ref().expect("ensure_display builds one");
        let keeps = |display: &str| anchor.is_none_or(|a| display.starts_with(a));

        if fuzzy::parse_query(pattern).is_none() {
            // Pre-sorted at cache build, so an unfiltered list is a walk rather
            // than a sort over freshly derived strings.
            let listed: Vec<usize> = cache
                .sorted
                .iter()
                .copied()
                .filter(|&idx| keeps(&cache.rows[idx]))
                .collect();

            self.match_indices = vec![(0..anchor_len).collect(); listed.len()];
            self.indexed = listed.len();
            self.scored = 0;
            self.filtered = listed;
            self.last_filter = Some((query.to_string(), self.base.len()));
            self.clamp_selected();
            return None;
        }

        let candidates = match arrived {
            Some(covered) => Candidates::Narrowed {
                previous: self.filtered.clone(),
                arrived: covered..cache.rows.len(),
            },
            None => Candidates::Every,
        };

        Some(Scan {
            query: query.to_string(),
            pattern: pattern.to_string(),
            anchor: anchor.map(str::to_string),
            anchor_len,
            rows: Arc::clone(&cache.rows),
            candidates,
            covered: self.base.len(),
        })
    }

    /// Take the rows a [`Scan`] produced as the current result.
    pub(crate) fn apply_scan(&mut self, outcome: ScanOutcome) {
        self.filtered = outcome.filtered;
        self.match_indices = outcome.match_indices;
        self.indexed = outcome.indexed;
        self.scored = outcome.scored;
        self.last_filter = Some((outcome.query, outcome.covered));
        self.clamp_selected();
        self.filter_generation = next_generation();
    }

    /// Highlight offsets for filtered row `row`.
    ///
    /// A row the refilter indexed returns its stored offsets. A row past that
    /// block was never indexed, so it is derived now into `scratch`, which a
    /// caller painting a window reuses across its rows.
    pub(crate) fn row_indices<'a>(&'a self, row: usize, scratch: &'a mut Vec<u32>) -> &'a [u32] {
        if let Some(indices) = self.match_indices.get(row) {
            return indices;
        }

        scratch.clear();
        let Some((query, _)) = self.last_filter.as_ref() else {
            return scratch;
        };
        let (Some(cache), Some(&idx)) = (self.display.as_ref(), self.filtered.get(row)) else {
            return scratch;
        };

        let (anchor, pattern) = split_root_anchor(query);
        let anchor_len = anchor.map_or(0, |a| a.chars().count()) as u32;
        fuzzy::indices_of(pattern, &cache.rows[idx], scratch);

        *scratch = prepend_anchor(anchor_len, std::mem::take(scratch));
        scratch
    }

    /// Whether `query` only narrows what [`Self::last_filter`] already matched,
    /// so the matcher can be handed [`Self::filtered`] instead of the whole
    /// base.
    ///
    /// The rows survive three separate conditions. The query has to extend the
    /// one behind them, since a shorter or divergent one can match rows they
    /// dropped. The `./` anchor has to have stayed absent or grown, a longer
    /// root prefix keeping strictly fewer rows. The pattern after the anchor
    /// has to extend the previous pattern and to be one that narrows at all,
    /// which [`fuzzy::extension_narrows`] decides.
    fn narrows_previous(&self, query: &str, anchor: Option<&str>, pattern: &str) -> bool {
        let Some((previous, _)) = self.last_filter.as_ref() else {
            return false;
        };
        if !query.starts_with(previous) {
            return false;
        }

        let (previous_anchor, previous_pattern) = split_root_anchor(previous);
        let anchor_narrows = match (previous_anchor, anchor) {
            (None, None) => true,
            (Some(was), Some(now)) => now.starts_with(was),
            _ => false,
        };

        anchor_narrows && pattern.starts_with(previous_pattern) && fuzzy::extension_narrows(pattern)
    }

    /// Point the list at a different candidate set, so the next
    /// [`Self::refilter`] derives its display strings afresh.
    pub(crate) fn set_base(&mut self, base: Vec<PathBuf>) {
        self.base = base;
        self.base_generation = next_generation();
    }

    /// Append candidates, leaving the ones already there in place.
    ///
    /// The display strings already built stay valid, and so does the current
    /// match, so a streaming walk costs a batch rather than the whole set it
    /// has collected so far.
    pub(crate) fn extend_base(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.base.extend(paths);
    }

    /// Build the display strings for the current base, reusing what still
    /// describes it.
    ///
    /// A walk that only appended leaves every row it already built valid, so
    /// the tail is derived and merged into the order rather than the whole set
    /// being rebuilt. Anything [`row_display`] reads moving, or the base being
    /// replaced outright, still rebuilds.
    fn ensure_display(&mut self, git_root: &Path) {
        // Resolved once for the whole list rather than per path, since
        // `row_display`'s git-root-relative branch would otherwise hit the env
        // for every candidate.
        let home = paths::home_dir();

        let describes_base = self.display.as_ref().is_some_and(|cache| {
            cache.base_generation == self.base_generation
                && cache.git_root == git_root
                && cache.display_roots == self.display_roots
                && cache.home == home
                && cache.rows.len() <= self.base.len()
        });

        if describes_base {
            let covered = self
                .display
                .as_ref()
                .expect("describes_base implies a cache")
                .rows
                .len();
            if covered < self.base.len() {
                self.extend_display(covered, git_root, home.as_deref());
            }
            return;
        }

        // The remembered rows are indices into the base being replaced, so they
        // name different paths on the other side of this rebuild.
        self.last_filter = None;

        let display_roots = self.display_roots.as_deref();
        let rows: Vec<Arc<str>> = self
            .base
            .iter()
            .map(|path| Arc::from(row_display(path, git_root, display_roots, home.as_deref())))
            .collect();

        let mut sorted: Vec<usize> = (0..rows.len()).collect();
        sorted.sort_by(|&a, &b| rows[a].cmp(&rows[b]));

        let generation = self
            .display
            .as_ref()
            .map_or(0, |cache| cache.generation + 1);
        self.display = Some(DisplayCache {
            base_generation: self.base_generation,
            git_root: git_root.to_path_buf(),
            display_roots: self.display_roots.clone(),
            home,
            rows: Arc::new(rows),
            sorted,
            generation,
        });
    }

    /// Derive display strings for `base[from..]` and fold them into the order.
    ///
    /// The tail is sorted on its own and merged against the rows already there.
    /// Ties take from the existing side, and every tail index is the larger, so
    /// the result is the order a full stable sort would have produced.
    fn extend_display(&mut self, from: usize, git_root: &Path, home: Option<&Path>) {
        let display_roots = self.display_roots.as_deref();
        let tail: Vec<Arc<str>> = self.base[from..]
            .iter()
            .map(|path| Arc::from(row_display(path, git_root, display_roots, home)))
            .collect();

        let mut tail_order: Vec<usize> = (from..from + tail.len()).collect();
        tail_order.sort_by(|&a, &b| tail[a - from].cmp(&tail[b - from]));

        let cache = self.display.as_mut().expect("the caller found a cache");
        Arc::make_mut(&mut cache.rows).extend(tail);

        let mut merged = Vec::with_capacity(cache.sorted.len() + tail_order.len());
        let mut order = cache.sorted.iter().copied().peekable();
        let mut arriving = tail_order.into_iter().peekable();
        loop {
            match (order.peek(), arriving.peek()) {
                (Some(&held), Some(&new)) => {
                    if cache.rows[held] <= cache.rows[new] {
                        merged.push(held);
                        order.next();
                    } else {
                        merged.push(new);
                        arriving.next();
                    }
                },
                (Some(_), None) => merged.extend(order.by_ref()),
                (None, Some(_)) => merged.extend(arriving.by_ref()),
                (None, None) => break,
            }
        }
        cache.sorted = merged;
    }

    /// Drop the current results, so the next [`Self::refilter`] starts from the
    /// whole base rather than narrowing rows that no longer stand for anything.
    pub(crate) fn clear_results(&mut self) {
        self.filtered.clear();
        self.match_indices.clear();
        self.last_filter = None;
    }

    fn clamp_selected(&mut self) {
        nav_clamp(self.filtered.len(), &mut self.selected);
    }
}

/// Split a leading `./` workspace-root anchor off `query`.
///
/// When the first whitespace-delimited token starts with `./`, returns that
/// token minus the `./` as a root-relative path prefix, plus the rest of the
/// query as the fuzzy pattern. A bare `./` yields an empty prefix, which every
/// path matches, so the finder lists all. A `./` in any but the first token is
/// ordinary fuzzy text and yields `(None, query)`.
fn split_root_anchor(query: &str) -> (Option<&str>, &str) {
    let after_ws = query.trim_start();
    let leading = query.len() - after_ws.len();
    let first_len = after_ws.find(char::is_whitespace).unwrap_or(after_ws.len());
    let Some(anchor) = after_ws[..first_len].strip_prefix("./") else {
        return (None, query);
    };
    (Some(anchor), query[leading + first_len..].trim_start())
}

/// Merge the `0..anchor_len` anchored-prefix character offsets into `matched`,
/// returning the sorted, deduplicated union so the renderer highlights the
/// pinned prefix alongside the fuzzy matches.
fn prepend_anchor(anchor_len: u32, matched: Vec<u32>) -> Vec<u32> {
    if anchor_len == 0 {
        return matched;
    }
    let mut indices: Vec<u32> = (0..anchor_len).chain(matched).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// The display string for a candidate path, used both as the fuzzy haystack and
/// the rendered row so the two never drift.
///
/// With `display_roots` set the path renders as `<owning-root basename>/<path
/// relative to that root>`, distinguishing same-named files across the
/// cross-workspace list. Otherwise it renders relative to the single `git_root`.
pub(crate) fn row_display(
    path: &Path,
    git_root: &Path,
    display_roots: Option<&[PathBuf]>,
    home: Option<&Path>,
) -> String {
    let mut out = String::new();
    write_row_display(&mut out, path, git_root, display_roots, home);
    out
}

/// Append [`row_display`]'s text for `path` to `out`.
///
/// For the row paint, which draws one candidate per visible row every frame and
/// would otherwise allocate a fresh string for each.
pub(crate) fn write_row_display(
    out: &mut String,
    path: &Path,
    git_root: &Path,
    display_roots: Option<&[PathBuf]>,
    home: Option<&Path>,
) {
    match display_roots {
        Some(roots) => out.push_str(&workspace_rooted_display(path, roots)),
        None => paths::write_display_relative_with_home(out, path, git_root, home),
    }
}

/// Render `path` under its owning workspace root as `<root basename>/<relative>`.
///
/// The owning root is the prefix of `path` with the longest path, so a nested
/// workspace root wins over an ancestor. Falls back to the path's own display
/// when no root contains it.
fn workspace_rooted_display(path: &Path, roots: &[PathBuf]) -> String {
    let owning = roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len());

    let Some(root) = owning else {
        return path.to_string_lossy().into_owned();
    };

    let base = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => base.to_string(),
        Ok(rel) => format!("{base}/{}", rel.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// How a [`PathPicker`] previews its current selection.
pub(crate) enum PreviewPolicy {
    /// Read the selected path from disk.
    File,
    /// Preview the live in-memory buffer when the path has one open, else the
    /// disk file. The finder's Buffers scope and the palette's buffer picker.
    LiveBufferThenFile,
    /// No preview -- e.g. a directory picker, which has nothing to show.
    NoPreview,
}

/// A walk-fed path list, its fuzzy [`PickList`], and a [`Preview`] pane.
///
/// Drives both the file finder and the palette's inline argument picker, so a
/// fix to walk draining, the refilter text-cache, or preview syncing reaches
/// both instead of only the copy it was written against.
pub(crate) struct PathPicker {
    pub(crate) git_root: PathBuf,
    /// Every candidate path. Grows as walk batches arrive via
    /// [`PathPicker::pump_walk`] for a walked source. A caller-fed source leaves
    /// it empty and drives [`PathPicker::refilter_with_base`] instead.
    pub(crate) all_paths: Vec<PathBuf>,
    walk_rx: Option<UnboundedReceiver<Vec<PathBuf>>>,
    _walk_task: Option<Task<()>>,
    pub(crate) picklist: PickList,
    /// Last query run through the matcher, so a render tick with no typing
    /// short-circuits. Cleared by [`PathPicker::invalidate`] when the base set
    /// changes under a stable query.
    pub(crate) last_filter_text: String,
    /// Whether [`Self::picklist`] reflects the current base and query. Set by a
    /// refilter, cleared by [`PathPicker::invalidate`]. Gating on this rather
    /// than a non-empty result keeps a zero-match query cached instead of
    /// refiltering the whole list every render tick.
    filter_valid: bool,
    /// Results from scans running elsewhere, each tagged with the generation it
    /// was started at.
    scan_rx: UnboundedReceiver<(u64, ScanOutcome)>,
    scan_tx: UnboundedSender<(u64, ScanOutcome)>,
    /// The generation of the most recently started scan.
    ///
    /// A result arriving under any other generation answers a query the user
    /// has since typed past, so it is dropped rather than painted. That is what
    /// keeps a burst of keystrokes from showing its own history in order.
    scan_generation: u64,
    /// Whether a scan is out and has not reported back yet.
    scan_pending: bool,
    /// Held so dropping the picker cancels a scan still running for it.
    _scan_task: Option<Task<()>>,
    /// How much of [`Self::all_paths`] the pick list's base already holds, so a
    /// refilter hands it only what arrived since.
    ///
    /// Dropped by [`PathPicker::invalidate`], which every caller that replaces
    /// [`Self::all_paths`] already goes through. The callers that only truncate
    /// it are covered without that, the length no longer agreeing.
    synced_paths: Option<usize>,
    /// The caller-owned base the last [`PathPicker::refilter_with_base`] was
    /// over, so the next one can tell whether it is the same set.
    ///
    /// Dropped by [`PathPicker::invalidate`], which is what a scope flip and
    /// every other real base change already go through, so a base swapped
    /// underneath a stable identity cannot read as unchanged.
    last_base: Option<BaseId>,
    pub(crate) preview: Preview,
}

impl PathPicker {
    /// Create a picker over `git_root`. `walk` is the streaming walker for a
    /// file source, or `None` for a caller-fed fixed set.
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        git_root: PathBuf,
        walk: Option<(UnboundedReceiver<Vec<PathBuf>>, Task<()>)>,
    ) -> Self {
        let (walk_rx, walk_task) = match walk {
            Some((rx, task)) => (Some(rx), Some(task)),
            None => (None, None),
        };
        let (scan_tx, scan_rx) = tokio::sync::mpsc::unbounded_channel();
        let preview = Preview::new(ws, executor);
        Self {
            git_root,
            all_paths: Vec::new(),
            walk_rx,
            _walk_task: walk_task,
            picklist: PickList::default(),
            last_filter_text: String::new(),
            filter_valid: false,
            scan_rx,
            scan_tx,
            scan_generation: 0,
            scan_pending: false,
            _scan_task: None,
            synced_paths: None,
            last_base: None,
            preview,
        }
    }

    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.picklist.selected_path()
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.picklist.move_selection(delta);
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picklist.page(dir);
    }

    /// Drain every walk batch since the last call into [`Self::all_paths`],
    /// invalidating the filter cache when any arrived. Returns whether a batch
    /// was consumed. No-op for a caller-fed source.
    pub(crate) fn pump_walk(&mut self) -> bool {
        let Some(rx) = self.walk_rx.as_mut() else {
            return false;
        };
        let mut received_any = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => {
                    self.all_paths.extend(batch);
                    received_any = true;
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.walk_rx = None;
                    break;
                },
            }
        }
        if received_any {
            // A batch only appends, so the pick list keeps its base, display
            // strings, and current match, and folds the new paths in on the
            // next refilter. Only the results are stale.
            self.filter_valid = false;
        }
        received_any
    }

    /// Force the next refilter to re-run the matcher, even under an unchanged
    /// query. Callers whose base set changed (a walk batch, a scope flip) call
    /// this so the stale filtered rows do not survive.
    pub(crate) fn invalidate(&mut self) {
        self.last_filter_text.clear();
        self.filter_valid = false;
        self.synced_paths = None;
        self.last_base = None;
        self.picklist.clear_results();
    }

    /// Re-root the walk. Clears the collected paths, drops the old walk task
    /// (cancelling the in-flight walk), and streams from `rx`/`task` instead.
    /// The filter cache is invalidated so the next refilter re-runs. Lets one
    /// browse picker follow the typed directory without re-allocating its
    /// preview buffers per keystroke.
    pub(crate) fn reset_walk(&mut self, rx: UnboundedReceiver<Vec<PathBuf>>, task: Task<()>) {
        self.all_paths.clear();
        self.walk_rx = Some(rx);
        self._walk_task = Some(task);
        self.invalidate();
    }

    /// Stop the walk, dropping the receiver and task so the background walk
    /// breaks on its next send. Bounds an otherwise unbounded walk once enough
    /// paths are collected.
    pub(crate) fn stop_walk(&mut self) {
        self.walk_rx = None;
        self._walk_task = None;
    }

    /// Refilter over this picker's own walk-fed [`Self::all_paths`], skipping
    /// the work when the query is unchanged and the filter is still valid.
    pub(crate) fn refilter(&mut self, query: &str) {
        if query == self.last_filter_text && self.filter_valid {
            return;
        }

        self.sync_base();
        self.run_refilter(query);
    }

    /// Bring the pick list's base up to date with the walk.
    ///
    /// A walk only ever appends, so the pick list is handed the paths that
    /// arrived since it last looked rather than a fresh copy of all of them.
    /// Copying every path per keystroke would also discard its display strings,
    /// which is the expensive half.
    fn sync_base(&mut self) {
        match self.synced_paths {
            Some(synced) if synced == self.all_paths.len() => {},
            Some(synced) if synced < self.all_paths.len() => {
                self.picklist
                    .extend_base(self.all_paths[synced..].iter().cloned());
            },
            _ => self.picklist.set_base(self.all_paths.clone()),
        }
        self.synced_paths = Some(self.all_paths.len());
    }

    /// Prepare the scan for `query`, for a caller that will run it elsewhere.
    ///
    /// Does everything a refilter does except the matching, and stamps what is
    /// left with a fresh generation. Hand the [`Scan`] and that generation to
    /// [`Self::scan_sink`]'s sender, and [`Self::pump_scan`] takes it from
    /// there. `None` means the query needed no scan and the list is already
    /// current, which an empty query is.
    ///
    /// The rows on display are untouched until a result lands, so a picker with
    /// a scan in flight keeps painting the query before it.
    pub(crate) fn begin_scan(&mut self, query: &str) -> Option<(u64, Scan)> {
        if query == self.last_filter_text && self.filter_valid {
            return None;
        }
        self.sync_base();

        // Marked valid on the way out rather than on arrival. It says a scan for
        // this query is accounted for, not that its answer is in hand.
        self.last_filter_text = query.to_string();
        self.filter_valid = true;

        let scan = self.picklist.begin_refilter(query, &self.git_root)?;
        self.scan_generation = next_generation();
        self.scan_pending = true;
        Some((self.scan_generation, scan))
    }

    /// The sender a scan's runner reports back through, and the task slot that
    /// keeps it alive.
    pub(crate) fn scan_sink(&mut self) -> UnboundedSender<(u64, ScanOutcome)> {
        self.scan_tx.clone()
    }

    /// Run a scan [`Self::begin_scan`] handed back on a worker.
    ///
    /// Matching and ranking a large base is too much to do inside the update
    /// path, where input and paint would wait on it. The rows on display stay
    /// as they are until [`Self::pump_scan`] takes the result, and the wake is
    /// what makes that happen without another keystroke, since the pumps poll
    /// with a noop waker.
    pub(crate) fn spawn_scan(
        &mut self,
        executor: &Executor,
        redraw: Arc<tokio::sync::Notify>,
        generation: u64,
        scan: Scan,
    ) {
        let sink = self.scan_sink();
        let task = executor.spawn_blocking(move || {
            let outcome = scan.run();
            if sink.send((generation, outcome)).is_ok() {
                redraw.notify_one();
            }
        });
        self.hold_scan(task);
    }

    /// [`Self::begin_scan`] over a caller-owned `base` rather than the walk.
    ///
    /// This leaves out the matching for the caller to run elsewhere, standing
    /// to [`Self::refilter_with_base`] as [`Self::begin_scan`] does to a plain
    /// refilter. The identity rules are that method's, so a base whose `id` has
    /// not moved keeps the rows already derived from it.
    pub(crate) fn begin_scan_with_base(
        &mut self,
        query: &str,
        base: &[PathBuf],
        id: BaseId,
    ) -> Option<(u64, Scan)> {
        if query == self.last_filter_text && self.filter_valid {
            return None;
        }

        self.adopt_base(base, id);
        // The pick list now holds a caller's set rather than a prefix of the
        // walk, so the next walk-fed refilter starts over.
        self.synced_paths = None;

        self.last_filter_text = query.to_string();
        self.filter_valid = true;

        let scan = self.picklist.begin_refilter(query, &self.git_root)?;
        self.scan_generation = next_generation();
        self.scan_pending = true;
        Some((self.scan_generation, scan))
    }

    /// Bring the rows up to date with `query` here and now.
    ///
    /// For a caller about to act on the selection rather than paint it. A scan
    /// running elsewhere answers the query that started it, and an action taken
    /// between keystrokes can already have moved past that, so acting on what is
    /// displayed would act on the wrong row.
    ///
    /// Costs one scan at the moment the user commits rather than one per
    /// keystroke, which is the whole point of the split. A result still in
    /// flight is orphaned rather than waited for.
    pub(crate) fn settle_scan(&mut self, query: &str) {
        self.pump_scan();
        if !self.scan_pending && query == self.last_filter_text && self.filter_valid {
            return;
        }

        self.sync_base();
        self.picklist.refilter(query, &self.git_root);
        self.last_filter_text = query.to_string();
        self.filter_valid = true;
        self.scan_pending = false;
        self.scan_generation = next_generation();
    }

    /// [`Self::settle_scan`] over a caller-owned `base` rather than the walk.
    ///
    /// A base-fed picker has to catch up against the set it was filtering, not
    /// the walk, which holds different paths and would answer a different
    /// question.
    pub(crate) fn settle_scan_with_base(&mut self, query: &str, base: &[PathBuf], id: BaseId) {
        self.pump_scan();
        if !self.scan_pending && query == self.last_filter_text && self.filter_valid {
            return;
        }

        self.adopt_base(base, id);
        self.synced_paths = None;
        self.run_refilter(query);
        self.scan_pending = false;
        self.scan_generation = next_generation();
    }

    /// Hold the task running a scan, so dropping the picker cancels it.
    pub(crate) fn hold_scan(&mut self, task: Task<()>) {
        self._scan_task = Some(task);
    }

    /// Take the newest arrived scan result, if it still answers the current
    /// query, and report whether the displayed rows moved.
    ///
    /// Results for superseded generations are drained and dropped. A burst of
    /// keystrokes therefore paints once, for the last of them, rather than
    /// flickering through the ones it outran.
    pub(crate) fn pump_scan(&mut self) -> bool {
        let mut current = None;
        loop {
            match self.scan_rx.try_recv() {
                Ok((generation, outcome)) if generation == self.scan_generation => {
                    current = Some(outcome);
                },
                Ok(_) => {},
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        match current {
            Some(outcome) => {
                self.picklist.apply_scan(outcome);
                self.scan_pending = false;
                true
            },
            None => false,
        }
    }

    /// Refilter over a caller-owned `base` set, which `id` names.
    ///
    /// A call carrying the identity the last one did is over the same set, so
    /// the pick list keeps the display strings it derived and the rows its
    /// previous query matched, and narrows those rather than rescoring the base.
    /// An `id` whose length grew adopts the tail alone.
    ///
    /// The query cache still applies, so a caller that changes `base` under a
    /// stable query must [`Self::invalidate`] first (the finder does this on a
    /// scope flip).
    pub(crate) fn refilter_with_base(&mut self, query: &str, base: &[PathBuf], id: BaseId) {
        if query == self.last_filter_text && self.filter_valid {
            return;
        }

        self.adopt_base(base, id);
        // The pick list now holds a caller's set rather than a prefix of the
        // walk, so the next walk-fed refilter starts over.
        self.synced_paths = None;

        self.run_refilter(query);
    }

    /// Point the pick list at `base`, keeping whatever of it still applies.
    ///
    /// Replacing the base is what costs. The display strings are re-derived and
    /// re-sorted per row, and the rows the previous query matched are forgotten,
    /// so the next one rescores the whole set. An identity that has not moved
    /// says none of that is needed.
    fn adopt_base(&mut self, base: &[PathBuf], id: BaseId) {
        let appended = self
            .last_base
            .is_some_and(|last| last.identity == id.identity && last.len <= id.len);

        match appended {
            true => {
                let held = self.last_base.expect("appended implies one").len;
                if held < id.len {
                    self.picklist.extend_base(base[held..].iter().cloned());
                }
            },
            false => self.picklist.set_base(base.to_vec()),
        }

        self.last_base = Some(id);
    }

    /// Run the matcher over whatever base the pick list now holds, and mark the
    /// filter valid so an unchanged-query re-render short-circuits until the
    /// next [`Self::invalidate`].
    fn run_refilter(&mut self, query: &str) {
        self.picklist.refilter(query, &self.git_root);
        self.last_filter_text = query.to_string();
        self.filter_valid = true;
    }

    /// Sync the preview pane to the current selection per `policy`, clearing it
    /// when nothing is selected.
    pub(crate) fn sync_preview(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
        policy: PreviewPolicy,
    ) {
        let Some(path) = self.selected_path().map(|p| p.to_path_buf()) else {
            self.preview.clear(ws);
            return;
        };
        let source = match policy {
            PreviewPolicy::File => Some(PreviewSource::File(path)),
            PreviewPolicy::LiveBufferThenFile => Some(match ws.buffers.id_for_path(&path) {
                Some(id) => PreviewSource::Buffer(id),
                None => PreviewSource::File(path),
            }),
            PreviewPolicy::NoPreview => None,
        };
        match source {
            Some(source) => self.preview.sync(ws, fs_host, language_registry, source),
            None => self.preview.clear(ws),
        }
    }

    /// Tear down the preview's owned editor slot. Callers dispose their own
    /// input widget separately.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.preview.dispose(ws);
    }
}

/// Where a [`Preview`] pulls its content from.
///
/// `File` reads the path from disk. `Buffer` reads a live, possibly modified
/// in-memory buffer, so the preview reflects unsaved edits rather than the
/// backing file.
#[derive(PartialEq)]
pub(crate) enum PreviewSource {
    File(PathBuf),
    /// Live in-memory buffer, so the preview reflects unsaved edits rather than
    /// the backing file. Used by the finder's Buffers scope and the palette's
    /// buffer argument picker.
    Buffer(BufferId),
}

/// Read-only preview pane backed by a reusable scratch buffer.
///
/// A picker drives this by calling [`Preview::sync`] with the selected source.
/// The scratch buffer's rope is replaced with that source's content and the
/// source's language is assigned so the parse pipeline highlights it.
pub(crate) struct Preview {
    pub(crate) editor: EditorId,
    pub(crate) buffer: BufferId,
    /// Source currently rendered into the scratch buffer, or `None` when empty.
    /// Lets [`Preview::sync`] skip a redundant reload when the selection is
    /// unchanged.
    rendered_for: Option<PreviewSource>,
}

impl Preview {
    /// Allocate the scratch preview buffer and its editor.
    pub(crate) fn new(ws: &mut Workspace, executor: Executor) -> Self {
        let (buffer, shared_buffer) = ws.buffers.new_scratch_preview();
        let editor_state =
            EditorState::new(buffer, shared_buffer, executor, ws.redraw_notify.clone());
        let editor = ws.editors.insert(editor_state);
        Self {
            editor,
            buffer,
            rendered_for: None,
        }
    }

    /// Load `source` into the scratch buffer, unless it is already shown.
    ///
    /// `File` reads disk through `fs_host` and resolves the language via
    /// `language_registry`. `Buffer` reads the live in-memory text and copies
    /// the source buffer's own language, ignoring both arguments. Stale syntax
    /// state is reset on every swap so an in-flight parse of the previous
    /// source cannot paint onto the new one. Read errors render a placeholder
    /// so the pane always shows something.
    pub(crate) fn sync(
        &mut self,
        ws: &mut Workspace,
        fs_host: &dyn FsHost,
        language_registry: &LanguageRegistry,
        source: PreviewSource,
    ) {
        if self.rendered_for.as_ref() == Some(&source) {
            return;
        }
        let (content, language) = match &source {
            PreviewSource::File(path) => (
                read_preview(fs_host, path),
                language_registry.for_path(path),
            ),
            PreviewSource::Buffer(id) => {
                let content = ws
                    .buffers
                    .get(*id)
                    .map(|b| {
                        b.read()
                            .expect("preview source buffer poisoned")
                            .rope()
                            .to_string()
                    })
                    .unwrap_or_default();
                (content, ws.buffers.language_for(*id))
            },
        };
        replace_preview_text(ws, self.editor, self.buffer, &content);
        ws.reset_preview_syntax(self.buffer);
        if let Some(language) = language {
            ws.buffers.set_language(self.buffer, language);
        }
        self.rendered_for = Some(source);
    }

    /// Blank the preview when nothing is selected. No-op when already empty.
    pub(crate) fn clear(&mut self, ws: &mut Workspace) {
        if self.rendered_for.is_some() {
            replace_preview_text(ws, self.editor, self.buffer, "");
            ws.reset_preview_syntax(self.buffer);
            self.rendered_for = None;
        }
    }

    /// Scroll the preview so 0-based `line` sits a few rows below the top, for a
    /// picker whose selection points at a line rather than a whole file.
    pub(crate) fn scroll_to_line(&self, ws: &mut Workspace, line: u32) {
        if let Some(editor) = ws.editors.get_mut(self.editor) {
            let scroll_row = line.saturating_sub(5);
            editor.scroll_row = scroll_row;
            editor.scroll_offset = scroll_row as f32;
            editor.scroll_glide = ScrollGlide::None;
        }
    }

    /// Remove the owned editor and scratch buffer from the workspace.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        ws.editors.remove(self.editor);
        ws.buffers.remove(self.buffer);
    }

    /// A preview holding no real workspace slots, for unit tests that construct
    /// a picker but never render or sync the pane.
    #[cfg(test)]
    pub(crate) fn test_dummy() -> Self {
        Self {
            editor: EditorId::default(),
            buffer: BufferId::new(0),
            rendered_for: None,
        }
    }
}

/// Read `path` through `fs_host`, truncating at [`PREVIEW_BYTE_LIMIT`] on a
/// UTF-8 char boundary. Returns a placeholder for read errors or non-UTF-8
/// content so the preview pane always renders. Output is run through
/// [`sanitize::sanitize_preview_text`] so unsanitized bytes never reach the
/// rope.
fn read_preview(fs_host: &dyn FsHost, path: &Path) -> String {
    let mut buf = Vec::new();
    if fs_host
        .read_prefix(path, PREVIEW_BYTE_LIMIT, &mut buf)
        .is_err()
    {
        return "<unreadable>".to_string();
    }
    let limit = PREVIEW_BYTE_LIMIT.min(buf.len());
    let raw = match std::str::from_utf8(&buf[..limit]) {
        Ok(s) => s.to_string(),
        Err(err) => {
            let valid = err.valid_up_to();
            String::from_utf8_lossy(&buf[..valid]).into_owned()
        },
    };
    sanitize::sanitize_preview_text(&raw)
}

/// Overwrite the preview scratch buffer with `text` and reset the editor's
/// viewport to the top.
fn replace_preview_text(ws: &mut Workspace, editor_id: EditorId, buffer_id: BufferId, text: &str) {
    let Some(buffer) = ws.buffers.get(buffer_id) else {
        return;
    };
    let old_len = {
        let guard = buffer.read().expect("preview buffer poisoned");
        guard.snapshot.visible_text.len()
    };
    {
        let mut guard = buffer.write().expect("preview buffer poisoned");
        guard.edit(0..old_len, text);
    }
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let anchor = buf_snap.anchor_at(0, Bias::Left);
        editor.selections.transform(buf_snap, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
        editor.scroll_row = 0;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Walk a cursor through a sequence of deltas, collecting where each one
    /// leaves it, so a whole traversal is one assertion.
    fn walk(len: usize, from: usize, deltas: &[i32]) -> Vec<usize> {
        let mut selected = from;
        deltas
            .iter()
            .map(|&delta| {
                nav_move(len, &mut selected, delta);
                selected
            })
            .collect()
    }

    #[test]
    fn the_cursor_stops_at_both_ends_rather_than_wrapping() {
        assert_eq!(
            walk(4, 0, &[1, 1, 1, 1, 1]),
            [1, 2, 3, 3, 3],
            "walking off the bottom parks on the last row"
        );
        assert_eq!(
            walk(4, 3, &[-1, -1, -1, -1, -1]),
            [2, 1, 0, 0, 0],
            "and off the top parks on the first"
        );
        assert_eq!(
            walk(4, 1, &[99, -99]),
            [3, 0],
            "a page-sized jump lands on the end it overshot"
        );
    }

    #[test]
    fn an_empty_list_holds_the_cursor_at_zero() {
        assert_eq!(walk(0, 7, &[1, -1, 50]), [0, 0, 0], "every move yields 0");

        let mut selected = 7;
        nav_clamp(0, &mut selected);
        assert_eq!(selected, 0, "and a clamp against no rows yields 0");
    }

    #[test]
    fn the_clamp_only_pulls_a_cursor_that_fell_outside() {
        let mut selected = 2;
        nav_clamp(4, &mut selected);
        assert_eq!(selected, 2, "a cursor inside the list stays put");

        let mut selected = 9;
        nav_clamp(4, &mut selected);
        assert_eq!(selected, 3, "one past the end lands on the last row");
    }

    #[test]
    fn a_page_covers_half_the_viewport_and_never_stalls() {
        assert_eq!(nav_page_step(Some(20)), 10, "an even viewport halves");
        assert_eq!(
            nav_page_step(Some(7)),
            4,
            "an odd one rounds up, so two pages clear the screen"
        );
        assert_eq!(
            nav_page_step(Some(1)),
            1,
            "a one-row viewport still moves a row"
        );
        assert_eq!(
            nav_page_step(Some(0)),
            1,
            "and so does a zero-row one, rather than paging nowhere"
        );
        assert_eq!(
            nav_page_step(None),
            1,
            "paging before the first render moves a single row"
        );
    }

    #[test]
    fn live_buffer_then_file_previews_disk_when_no_buffer() {
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let language_registry = h.stoat.language_registry.clone();
        let fs = crate::host::FakeFs::new();
        fs.insert_files(std::iter::once((
            p("/repo/on_disk.txt"),
            b"disk content\n".as_slice(),
        )));

        let ws = h.stoat.active_workspace_mut();
        let mut picker = PathPicker::new(ws, executor, p("/repo"), None);
        picker.all_paths = vec![p("/repo/on_disk.txt")];
        picker.refilter("");

        // The selected path has no open buffer, so the unified LiveBufferThenFile
        // policy -- the palette's buffer picker among them -- falls back to disk
        // rather than clearing the pane.
        picker.sync_preview(
            ws,
            &fs,
            &language_registry,
            PreviewPolicy::LiveBufferThenFile,
        );

        let shown = {
            let buffer = ws
                .buffers
                .get(picker.preview.buffer)
                .expect("preview buffer");
            let guard = buffer.read().expect("preview buffer poisoned");
            guard.rope().to_string()
        };
        assert!(
            shown.contains("disk content"),
            "no-buffer path falls back to the disk file, got {shown:?}"
        );

        picker.dispose(ws);
    }

    /// Display strings of the filtered rows after running `query` over `base`.
    fn names(query: &str, base: Vec<PathBuf>, git_root: &Path) -> Vec<String> {
        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter(query, git_root);
        list.filtered
            .iter()
            .map(|i| paths::display_relative(&list.base[*i], git_root))
            .collect()
    }

    /// The generation of the cache backing `list`, or `None` before one is
    /// built.
    fn display_generation(list: &PickList) -> Option<u64> {
        list.display.as_ref().map(|cache| cache.generation)
    }

    /// Deriving a display string relativizes a path and allocates, and the
    /// filter needs every one of them per keystroke, so typing must reuse them.
    #[test]
    fn refiltering_an_unchanged_base_reuses_the_display_strings() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/b.rs"), p("/r/a.rs"), p("/r/sub/c.rs")],
            ..PickList::default()
        };

        list.refilter("", &git_root);
        let built = display_generation(&list).expect("the first refilter builds a cache");

        for query in ["a", "a.", "a.r", "a.rs", ""] {
            list.refilter(query, &git_root);
            assert_eq!(
                display_generation(&list),
                Some(built),
                "typing {query:?} over the same base must not rebuild the strings",
            );
        }
    }

    /// The cross-workspace scope re-roots how a path renders without touching
    /// the path list, so the cache cannot key on the base alone.
    #[test]
    fn flipping_display_roots_rebuilds_the_display_strings() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/a.rs")],
            ..PickList::default()
        };

        list.refilter("", &git_root);
        let built = display_generation(&list).expect("the first refilter builds a cache");
        assert_eq!(
            names_of(&list),
            vec!["a.rs"],
            "rendered against the git root"
        );

        list.display_roots = Some(vec![p("/r")]);
        list.refilter("", &git_root);

        assert_ne!(
            display_generation(&list),
            Some(built),
            "a re-rooted list renders differently and must be rebuilt",
        );
        assert_eq!(
            names_of(&list),
            vec!["r/a.rs"],
            "rows now carry their owning root's basename",
        );
    }

    /// The display strings the filtered rows would paint, in filtered order.
    fn names_of(list: &PickList) -> Vec<String> {
        let cache = list.display.as_ref().expect("a cache is built");
        list.filtered
            .iter()
            .map(|&idx| cache.rows[idx].to_string())
            .collect()
    }

    #[test]
    fn empty_input_lists_all_base_paths_sorted() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs"), p("/r/sub/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "b.rs", "sub/c.rs"]);
    }

    #[test]
    fn refilter_and_construction_bump_the_filter_generation() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/a.rs"), p("/r/b.rs")],
            ..PickList::default()
        };
        let before = list.filter_generation;
        list.refilter("a", &git_root);
        assert_ne!(
            list.filter_generation, before,
            "a refilter stamps a fresh generation",
        );
        assert_ne!(
            PickList::default().filter_generation,
            list.filter_generation,
            "a freshly constructed list gets a distinct generation",
        );
    }

    #[test]
    fn prefix_ranks_before_substring_before_fuzzy() {
        let git_root = p("/r");
        let base = vec![
            p("/r/file.rs"),      // prefix
            p("/r/sub/file.rs"),  // substring
            p("/r/fee/nile.rs"),  // fuzzy (f..i..l..e)
            p("/r/unrelated.rs"), // filtered out
        ];
        assert_eq!(
            names("file", base, &git_root),
            vec!["file.rs", "sub/file.rs", "fee/nile.rs"]
        );
    }

    #[test]
    fn case_insensitive_filter() {
        let git_root = p("/r");
        let base = vec![p("/r/Foo.rs"), p("/r/bar.rs")];
        assert_eq!(names("foo", base, &git_root), vec!["Foo.rs"]);
    }

    #[test]
    fn root_anchor_lists_only_prefixed_paths() {
        let git_root = p("/r");
        let base = vec![p("/r/docs/a.md"), p("/r/src/b.rs")];
        assert_eq!(names("./docs", base, &git_root), vec!["docs/a.md"]);
    }

    #[test]
    fn root_anchor_matches_a_partial_prefix() {
        let git_root = p("/r");
        let base = vec![p("/r/docs/a.md"), p("/r/docs/b.md"), p("/r/src/c.rs")];
        assert_eq!(
            names("./do", base, &git_root),
            vec!["docs/a.md", "docs/b.md"]
        );
    }

    #[test]
    fn root_anchor_narrows_with_a_trailing_pattern() {
        let git_root = p("/r");
        let base = vec![
            p("/r/docs/readme.md"),
            p("/r/docs/other.md"),
            p("/r/src/x.rs"),
        ];
        assert_eq!(names("./docs rea", base, &git_root), vec!["docs/readme.md"]);
    }

    #[test]
    fn bare_root_anchor_lists_all_sorted() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs")];
        assert_eq!(names("./", base, &git_root), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn root_anchor_highlights_the_prefix() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/docs/readme.md")],
            ..PickList::default()
        };
        list.refilter("./docs", &git_root);
        assert_eq!(list.match_indices, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn split_root_anchor_only_anchors_the_first_token() {
        assert_eq!(split_root_anchor("./docs"), (Some("docs"), ""));
        assert_eq!(split_root_anchor("./docs rea"), (Some("docs"), "rea"));
        assert_eq!(split_root_anchor("./"), (Some(""), ""));
        assert_eq!(split_root_anchor("foo ./docs"), (None, "foo ./docs"));
        assert_eq!(split_root_anchor("foo"), (None, "foo"));
    }

    #[test]
    fn trailing_space_does_not_eliminate_matches() {
        let git_root = p("/r");
        let base = vec![p("/r/foo.rs"), p("/r/bar.rs")];
        assert_eq!(names(".rs ", base, &git_root), vec!["bar.rs", "foo.rs"]);
    }

    #[test]
    fn multi_token_query_matches_in_either_order() {
        let git_root = p("/r");
        let base = vec![p("/r/src/foo.rs"), p("/r/src/bar.rs")];
        let forward = names(".rs foo", base.clone(), &git_root);
        let reverse = names("foo .rs", base, &git_root);
        assert_eq!(forward, vec!["src/foo.rs"]);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn whitespace_only_query_lists_all_paths() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs")];
        assert_eq!(names("   ", base, &git_root), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn exact_basename_match_outranks_longer_prefix_match() {
        let git_root = p("/r");
        let base = vec![p("/r/food_handler.rs"), p("/r/foo.rs")];
        assert_eq!(
            names("foo", base, &git_root),
            vec!["foo.rs", "food_handler.rs"]
        );
    }

    #[test]
    fn filters_against_a_subset_base() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs")];
        assert_eq!(names("", base, &git_root), vec!["b.rs"]);
    }

    #[test]
    fn empty_base_lists_nothing() {
        let git_root = p("/r");
        assert!(names("", vec![], &git_root).is_empty());
    }

    #[test]
    fn lists_every_base_path_on_empty_query() {
        let git_root = p("/r");
        let base = vec![p("/r/a.rs"), p("/r/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "c.rs"]);
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")],
            selected: 2,
            ..PickList::default()
        };
        list.refilter("b", &git_root);
        assert_eq!(list.filtered.len(), 1);
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn row_display_defaults_to_git_relative() {
        assert_eq!(
            row_display(&p("/r/src/main.rs"), &p("/r"), None, None),
            "src/main.rs"
        );
    }

    #[test]
    fn workspace_rooted_display_prefixes_the_owning_root_basename() {
        let roots = vec![p("/a/proj"), p("/b/other")];
        let ignored = p("/ignored");
        assert_eq!(
            row_display(&p("/a/proj/src/main.rs"), &ignored, Some(&roots), None),
            "proj/src/main.rs"
        );
        assert_eq!(
            row_display(&p("/b/other/lib.rs"), &ignored, Some(&roots), None),
            "other/lib.rs"
        );
    }

    #[test]
    fn workspace_rooted_display_picks_the_longest_prefix_root() {
        let roots = vec![p("/a"), p("/a/nested")];
        assert_eq!(
            row_display(&p("/a/nested/file.rs"), &p("/ignored"), Some(&roots), None),
            "nested/file.rs",
            "a nested workspace root wins over its ancestor",
        );
    }

    #[test]
    fn workspace_rooted_display_falls_back_when_no_root_contains_the_path() {
        let roots = vec![p("/a/proj")];
        assert_eq!(
            row_display(&p("/elsewhere/x.rs"), &p("/ignored"), Some(&roots), None),
            "/elsewhere/x.rs"
        );
    }

    /// A candidate set with enough shared substructure that most queries keep
    /// some rows and drop others, which is where a wrong subset shows up.
    fn narrowing_base() -> Vec<PathBuf> {
        let mut base: Vec<PathBuf> = Vec::new();
        for dir in ["src", "srv", "Src", "tests", "docs/deep"] {
            for stem in ["main", "Main", "lib", "loader", "mod", "read_me"] {
                for ext in ["rs", "md"] {
                    base.push(p(&format!("/repo/{dir}/{stem}.{ext}")));
                }
            }
        }
        base
    }

    fn list_over(base: &[PathBuf]) -> PickList {
        PickList {
            base: base.to_vec(),
            ..PickList::default()
        }
    }

    /// The rows and highlight offsets a query produces with no history behind
    /// it, which is what narrowing has to reproduce exactly.
    fn from_scratch(base: &[PathBuf], query: &str) -> (Vec<usize>, Vec<Vec<u32>>) {
        let mut list = list_over(base);
        list.refilter(query, &p("/repo"));
        (list.filtered.clone(), list.match_indices.clone())
    }

    #[test]
    fn typing_a_query_out_matches_filtering_it_from_scratch() {
        // Covers the anchor, negation, and escape characters alongside ordinary
        // text, so the queries that must refuse to narrow are exercised too.
        const ALPHABET: [char; 14] = [
            'm', 'a', 'i', 'n', 'r', 's', 'd', 'M', '.', '/', ' ', '!', '\\', '\'',
        ];

        let base = narrowing_base();
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        for _ in 0..200 {
            let mut list = list_over(&base);
            let mut query = String::new();

            for _ in 0..6 {
                query.push(ALPHABET[next() % ALPHABET.len()]);
                list.refilter(&query, &p("/repo"));

                let (rows, indices) = from_scratch(&base, &query);
                assert_eq!(list.filtered, rows, "rows differ for query {query:?}");
                assert_eq!(
                    list.match_indices, indices,
                    "highlights differ for query {query:?}"
                );
            }
        }
    }

    #[test]
    fn deleting_from_a_query_refilters_the_whole_base() {
        let base = narrowing_base();
        let mut list = list_over(&base);

        list.refilter("mainrs", &p("/repo"));
        list.refilter("main", &p("/repo"));

        assert_eq!(
            list.scored,
            base.len(),
            "a shrunk query can match rows the longer one dropped, so it rescans"
        );
        assert_eq!(
            list.filtered,
            from_scratch(&base, "main").0,
            "and lands on the same rows as a fresh filter"
        );
    }

    #[test]
    fn extending_a_query_scores_only_the_rows_that_matched() {
        let base = narrowing_base();
        let mut list = list_over(&base);

        list.refilter("m", &p("/repo"));
        let after_first = list.filtered.len();
        assert_eq!(list.scored, base.len(), "the first query has no history");

        list.refilter("ma", &p("/repo"));
        assert_eq!(
            list.scored, after_first,
            "the second scores what the first matched, not the base"
        );
        assert!(
            after_first < base.len(),
            "the fixture has to drop rows for that to mean anything"
        );
    }

    #[test]
    fn extending_a_negated_query_rescans_the_base() {
        let base = narrowing_base();
        let mut list = list_over(&base);

        list.refilter("!ma", &p("/repo"));
        list.refilter("!mai", &p("/repo"));

        assert_eq!(
            list.scored,
            base.len(),
            "a negation matches more rows as it grows, so its rows cannot seed it"
        );
        assert_eq!(
            list.filtered,
            from_scratch(&base, "!mai").0,
            "and the result is the full-scan one"
        );
    }

    /// The same paths handed over at once, and streamed in `chunks` batches
    /// with a refilter after each, which is what a walk does.
    fn one_shot_and_streamed(base: &[PathBuf], chunks: usize, query: &str) -> (PickList, PickList) {
        let root = p("/repo");

        let mut one_shot = PickList::default();
        one_shot.set_base(base.to_vec());
        one_shot.refilter(query, &root);

        let mut streamed = PickList::default();
        for batch in base.chunks(base.len().div_ceil(chunks)) {
            streamed.extend_base(batch.iter().cloned());
            streamed.refilter(query, &root);
        }

        (one_shot, streamed)
    }

    fn assert_indistinguishable(one_shot: &PickList, streamed: &PickList, what: &str) {
        let whole = one_shot.display.as_ref().expect("a cache");
        let grown = streamed.display.as_ref().expect("a cache");

        assert_eq!(grown.rows, whole.rows, "rows differ {what}");
        assert_eq!(
            grown.sorted, whole.sorted,
            "unfiltered order differs {what}"
        );
        assert_eq!(
            streamed.filtered, one_shot.filtered,
            "matches differ {what}"
        );
        assert_eq!(
            streamed.match_indices, one_shot.match_indices,
            "highlights differ {what}"
        );
    }

    #[test]
    fn a_streamed_walk_lands_where_the_whole_set_would_have() {
        let base = narrowing_base();

        for query in ["", "main", "ma rs", "./src", "./src ma", "!main"] {
            for chunks in [2, 3, 7] {
                let (one_shot, streamed) = one_shot_and_streamed(&base, chunks, query);
                assert_indistinguishable(
                    &one_shot,
                    &streamed,
                    &format!("for {query:?} across {chunks} batches"),
                );
            }
        }
    }

    #[test]
    fn the_merged_order_breaks_ties_where_a_full_sort_would() {
        // Duplicated paths give every display string a twin, so the merge has to
        // choose a side on every comparison rather than never meeting a tie.
        let mut base = narrowing_base();
        base.extend(base.clone());

        let (one_shot, streamed) = one_shot_and_streamed(&base, 4, "");
        assert_indistinguishable(&one_shot, &streamed, "across rows that tie");
    }

    #[test]
    fn appending_while_the_rows_are_shared_copies_no_strings() {
        let base = narrowing_base();
        let head = base.len() / 2;

        let mut list = list_over(&base[..head]);
        list.refilter("ma", &p("/repo"));

        // What a background scan holds for the length of its run.
        let held = list.display.as_ref().expect("a cache").rows.clone();

        list.extend_base(base[head..].iter().cloned());
        list.refilter("ma", &p("/repo"));

        assert_eq!(
            held.len(),
            head,
            "the scan's view keeps the rows it started with"
        );

        let grown = list.display.as_ref().expect("a cache");
        assert_eq!(grown.rows.len(), base.len(), "and the list gains the rest");
        assert!(
            held.iter()
                .zip(grown.rows.iter())
                .all(|(before, after)| Arc::ptr_eq(before, after)),
            "the shared prefix is the same strings rather than copies of them"
        );
    }

    #[test]
    fn a_display_roots_flip_rebuilds_every_row() {
        let mut list = PickList::default();
        list.set_base(vec![p("/a/x/one.rs"), p("/b/y/two.rs")]);
        list.refilter("", &p("/a"));

        let (built, rows) = {
            let cache = list.display.as_ref().expect("a cache");
            (cache.generation, cache.rows.clone())
        };

        list.display_roots = Some(vec![p("/a/x"), p("/b/y")]);
        list.refilter("", &p("/a"));

        let cache = list.display.as_ref().expect("a cache");
        assert_ne!(
            cache.generation, built,
            "the flip rewrites every row, so extending would keep stale ones"
        );
        assert_ne!(cache.rows, rows, "and the rows do change");
    }

    #[test]
    fn a_walk_batch_scores_only_the_paths_it_brought() {
        let base = narrowing_base();
        let walked = base.len() / 2;

        let mut list = PickList::default();
        list.set_base(base[..walked].to_vec());
        list.refilter("ma", &p("/repo"));
        let matched = list.filtered.len();

        list.extend_base(base[walked..].iter().cloned());
        list.refilter("ma", &p("/repo"));

        assert_eq!(
            list.scored,
            matched + (base.len() - walked),
            "the arriving batch plus what already matched, not the whole base"
        );
        assert!(
            matched < walked,
            "the fixture has to drop rows for that to mean anything"
        );
    }

    #[test]
    fn a_grown_path_set_keeps_the_display_cache() {
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let mut picker = PathPicker::new(ws, executor, p("/repo"), None);

        picker.all_paths = vec![p("/repo/one.rs"), p("/repo/two.rs")];
        picker.refilter("r");
        let built = picker
            .picklist
            .display
            .as_ref()
            .expect("a cache")
            .generation;

        picker.all_paths.push(p("/repo/three.rs"));
        // A walk batch leaves the results stale and everything else intact.
        picker.filter_valid = false;
        picker.refilter("r");

        let cache = picker.picklist.display.as_ref().expect("a cache");
        assert_eq!(
            cache.generation, built,
            "the batch extended the rows rather than rebuilding them"
        );
        assert_eq!(cache.rows.len(), 3, "and the arriving path has a row");
    }

    /// A capped scope hands over the same list on every keystroke, so what it
    /// costs turns on whether the picker can tell. Rebuilding re-derives and
    /// re-sorts a row per path and forgets what the shorter query matched.
    #[test]
    fn a_base_handed_over_again_keeps_the_display_cache() {
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let mut picker = PathPicker::new(ws, executor, p("/repo"), None);

        let base = vec![p("/repo/one.rs"), p("/repo/two.rs")];
        let id = BaseId {
            identity: 7,
            len: base.len(),
        };
        picker.refilter_with_base("o", &base, id);
        let built = picker
            .picklist
            .display
            .as_ref()
            .expect("a cache")
            .generation;

        picker.refilter_with_base("on", &base, id);
        assert_eq!(
            picker
                .picklist
                .display
                .as_ref()
                .expect("a cache")
                .generation,
            built,
            "the same base is the same base, whatever the query did"
        );

        // The set grew, which is what a streaming named scope does between
        // keystrokes. Its rows still describe what they described. A walk batch
        // is what stales the results without touching anything else, and is why
        // the unchanged query still reaches the base below.
        let grown = vec![p("/repo/one.rs"), p("/repo/two.rs"), p("/repo/on_top.rs")];
        picker.filter_valid = false;
        picker.refilter_with_base(
            "on",
            &grown,
            BaseId {
                identity: 7,
                len: grown.len(),
            },
        );
        let cache = picker.picklist.display.as_ref().expect("a cache");
        assert_eq!(
            cache.generation, built,
            "growing extended the rows rather than rebuilding them"
        );
        assert_eq!(cache.rows.len(), 3, "and the arriving path has a row");

        // A different list under the same length, which is the case an identity
        // exists to catch.
        picker.filter_valid = false;
        picker.refilter_with_base(
            "on",
            &grown,
            BaseId {
                identity: 8,
                len: grown.len(),
            },
        );
        assert_ne!(
            picker
                .picklist
                .display
                .as_ref()
                .expect("a cache")
                .generation,
            built,
            "a base that is not the one held has to be rebuilt"
        );
    }

    /// A candidate set deeper than the eagerly indexed block, every row of which
    /// matches, so rows genuinely fall past it.
    fn deeper_than_the_block() -> Vec<PathBuf> {
        (0..INDEXED_ROWS + 64)
            .map(|i| p(&format!("/repo/src/module_{i:04}_main.rs")))
            .collect()
    }

    #[test]
    fn rows_past_the_indexed_block_derive_their_offsets_on_demand() {
        let base = deeper_than_the_block();
        let mut list = list_over(&base);
        list.refilter("main", &p("/repo"));

        assert_eq!(
            list.filtered.len(),
            base.len(),
            "every row of the fixture matches"
        );
        assert_eq!(
            list.indexed, INDEXED_ROWS,
            "only the block is indexed up front"
        );
        assert_eq!(
            list.match_indices.len(),
            INDEXED_ROWS,
            "and only the block stores offsets"
        );

        let deep = INDEXED_ROWS + 10;
        let row = {
            let cache = list.display.as_ref().expect("a cache");
            cache.rows[list.filtered[deep]].clone()
        };

        let stored = {
            let mut unbounded = fuzzy::match_and_rank("main", std::iter::once((0usize, &*row)))
                .expect("the query has atoms");
            unbounded.pop().expect("the row matches").matched_indices
        };

        let mut derived = Vec::new();
        assert_eq!(
            list.row_indices(deep, &mut derived),
            stored.as_slice(),
            "deriving late gives what indexing early would have stored"
        );
    }

    #[test]
    fn a_recomputed_row_keeps_its_anchor_highlight() {
        let base = deeper_than_the_block();
        let mut list = list_over(&base);
        list.refilter("./src main", &p("/repo"));

        let deep = INDEXED_ROWS + 10;
        let mut derived = Vec::new();
        let indices = list.row_indices(deep, &mut derived).to_vec();

        assert_eq!(
            &indices[..3],
            &[0, 1, 2],
            "the anchor's own characters stay highlighted on a recomputed row"
        );
        assert!(
            indices.len() > 3,
            "and the pattern's own matches follow them"
        );
    }

    fn walked_picker(h: &mut crate::test_harness::TestHarness) -> PathPicker {
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let mut picker = PathPicker::new(ws, executor, p("/repo"), None);
        picker.all_paths = narrowing_base();
        picker
    }

    fn answered_query(picker: &PathPicker) -> Option<&str> {
        picker
            .picklist
            .last_filter
            .as_ref()
            .map(|(query, _)| query.as_str())
    }

    #[test]
    fn a_burst_of_queries_lands_only_the_last() {
        let mut h = crate::Stoat::test();
        let mut picker = walked_picker(&mut h);

        let scans: Vec<_> = ["m", "ma", "mai"]
            .into_iter()
            .map(|query| picker.begin_scan(query).expect("each query needs a scan"))
            .collect();

        let sink = picker.scan_sink();
        for (generation, scan) in scans {
            sink.send((generation, scan.run()))
                .expect("the picker is listening");
        }

        assert!(picker.pump_scan(), "a result landed");
        assert_eq!(
            answered_query(&picker),
            Some("mai"),
            "the rows answer the last query typed, not one it outran"
        );
    }

    #[test]
    fn a_scan_finishing_after_its_query_moved_on_is_dropped() {
        let mut h = crate::Stoat::test();
        let mut picker = walked_picker(&mut h);

        let (slow, first) = picker.begin_scan("m").expect("a scan");
        let (quick, second) = picker.begin_scan("ma").expect("a scan");

        let sink = picker.scan_sink();
        sink.send((quick, second.run())).expect("listening");
        assert!(picker.pump_scan());
        assert_eq!(answered_query(&picker), Some("ma"));

        // The earlier scan reports back late, having taken longer over a query
        // the user has since typed past.
        sink.send((slow, first.run())).expect("listening");
        assert!(
            !picker.pump_scan(),
            "the late result changes nothing rather than reverting the list"
        );
        assert_eq!(answered_query(&picker), Some("ma"));
    }

    /// Deferring the matching over a caller's base has to reach the same rows
    /// as doing it inline, or a scope that starts scanning starts answering
    /// differently.
    #[test]
    fn a_deferred_base_scan_lands_where_an_inline_one_does() {
        let mut h = crate::Stoat::test();
        let base = narrowing_base();
        let id = BaseId {
            identity: 7,
            len: base.len(),
        };

        let inline = {
            let mut picker = walked_picker(&mut h);
            picker.refilter_with_base("main", &base, id);
            picker.picklist.filtered.clone()
        };

        let mut picker = walked_picker(&mut h);
        let (generation, scan) = picker
            .begin_scan_with_base("main", &base, id)
            .expect("a scan");
        let sink = picker.scan_sink();
        sink.send((generation, scan.run())).expect("listening");
        assert!(picker.pump_scan(), "the result landed");

        assert_eq!(picker.picklist.filtered, inline);
        assert!(!inline.is_empty(), "and the query matched something");
    }

    /// Settling a base-fed picker catches up against the base it was filtering.
    ///
    /// The walk holds a different set, so settling against that would answer a
    /// question the picker was never asked. The two are deliberately disjoint
    /// here, which is what makes the wrong one visible.
    #[test]
    fn settling_a_base_fed_picker_uses_that_base_not_the_walk() {
        let mut h = crate::Stoat::test();
        let mut picker = walked_picker(&mut h);

        let base = vec![p("/repo/scoped/keeper.rs"), p("/repo/scoped/other.rs")];
        let id = BaseId {
            identity: 11,
            len: base.len(),
        };
        let (_, _scan) = picker
            .begin_scan_with_base("keep", &base, id)
            .expect("a scan");

        // The typed query moved on before the scan could land.
        picker.settle_scan_with_base("keeper", &base, id);

        let rows: Vec<&PathBuf> = picker
            .picklist
            .filtered
            .iter()
            .map(|&i| &picker.picklist.base[i])
            .collect();
        assert_eq!(
            rows,
            vec![&p("/repo/scoped/keeper.rs")],
            "the row comes from the caller's base, which the walk does not hold",
        );
    }

    #[test]
    fn settling_takes_the_rows_the_typed_query_wants() {
        let mut h = crate::Stoat::test();
        let mut picker = walked_picker(&mut h);

        // A scan is out for a query the user has already typed past.
        let (orphaned, scan) = picker.begin_scan("m").expect("a scan");

        picker.settle_scan("mai");
        assert_eq!(
            answered_query(&picker),
            Some("mai"),
            "settling answers what is typed rather than waiting on what is out"
        );

        let sink = picker.scan_sink();
        sink.send((orphaned, scan.run())).expect("listening");
        assert!(
            !picker.pump_scan(),
            "and the scan it overtook can no longer land"
        );
    }

    #[test]
    fn a_query_that_gains_a_root_anchor_does_not_narrow() {
        let base = narrowing_base();
        let mut list = list_over(&base);
        list.refilter(".", &p("/repo"));

        let (anchor, pattern) = split_root_anchor("./s .");
        assert!(
            !list.narrows_previous("./s .", anchor, pattern),
            "an anchor appearing restricts rows by a rule the previous query never applied"
        );
    }

    #[test]
    fn a_walked_in_base_rescans_rather_than_narrowing_stale_rows() {
        let base = narrowing_base();
        let mut list = list_over(&base);
        list.refilter("m", &p("/repo"));

        list.set_base(base[..base.len() / 2].to_vec());
        list.refilter("ma", &p("/repo"));

        assert_eq!(
            list.scored,
            base.len() / 2,
            "the rows named different paths under the old base, so they are dropped"
        );
        assert_eq!(list.filtered, from_scratch(&base[..base.len() / 2], "ma").0);
    }
}
