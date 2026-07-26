use crate::{
    commit_list::PendingPreview,
    fuzzy,
    host::CommitInfo,
    input_view::{InputView, SubmitTarget},
    review_session::ReviewSession,
    workspace::Workspace,
};
use std::{collections::HashMap, mem, path::PathBuf, sync::Arc};
use stoat_scheduler::Executor;

/// What selecting a row in a [`CommitPicker`] does.
///
/// The rows, filtering, and preview are the same whatever the picker is being
/// used for, so a future commit-listing surface adds a variant here and a
/// select semantic rather than a second picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitPickerRole {
    /// Choose the commit a review walk starts from.
    PickBase,
    /// Read-only listing. Selecting a row dismisses the picker without
    /// touching the working tree.
    Browse,
}

/// Columns of the commit table, in display order.
///
/// The order is also the order they are joined into a row's haystack, so a
/// query reads left to right the way the table does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitColumn {
    Commit,
    Branch,
    Title,
    Author,
    Date,
}

/// How many columns [`CommitRow`] carries, indexable by `CommitColumn as usize`.
pub(crate) const COMMIT_COLUMNS: usize = 5;

/// One cell of a [`CommitRow`], and where it sits in the row's haystack.
pub(crate) struct CommitCell {
    pub(crate) text: String,
    /// Character offset of this cell's first character within
    /// [`CommitRow::text`], so a match offset resolves to a cell and a position
    /// inside it.
    pub(crate) start: usize,
}

/// One rendered picker row, holding both its cells and the joined text they
/// came from.
///
/// The join is the fuzzy haystack, so a query still matches across the whole
/// row and a reported match offset is an offset into [`Self::text`]. The cells
/// carry their own start offsets so the renderer can put that offset back on
/// the column showing it. See [`CommitPicker::row`].
pub(crate) struct CommitRow {
    pub(crate) text: String,
    pub(crate) cells: [CommitCell; COMMIT_COLUMNS],
}

/// Seconds in each unit an age is reported in, largest first, so a difference
/// takes the coarsest unit it fills.
///
/// A month is a plain thirtieth of a year rather than a calendar month, which
/// is what lets this stay a pure integer bucket with no date library behind it.
/// The label is approximate by design. "3mo" tells the reader what they need
/// about a commit's age, where an exact date would not.
const AGE_UNITS: [(i64, &str); 5] = [
    (31_536_000, "y"),
    (2_592_000, "mo"),
    (86_400, "d"),
    (3_600, "h"),
    (60, "m"),
];

/// A commit's age at `now`, as a short relative label like `3d` or `2mo`.
///
/// Anything under a minute, and anything dated in the future (a skewed clock,
/// a rewritten author date), reads as `now` rather than a negative age.
pub(crate) fn age_label(now_epoch: i64, commit_time: i64) -> String {
    let age = now_epoch.saturating_sub(commit_time);
    for (seconds, suffix) in AGE_UNITS {
        if age >= seconds {
            return format!("{}{suffix}", age / seconds);
        }
    }
    "now".to_string()
}

/// A picker scope a drill-in displaced, kept so popping back restores exactly
/// what the user was looking at.
///
/// Drilling into a merge replaces the whole list, and the user expects Alt-Left
/// to undo that completely rather than dropping them at the top of the old list
/// with their query gone. Everything a drill overwrites is saved here.
///
/// Nothing in the crate drills yet, so the scope machinery reads as dead until
/// the drill actions that call [`CommitPicker::push_scope`] land.
#[allow(dead_code)]
pub(crate) struct CommitScope {
    /// The displaced scope's own label, `None` when it was the root scope whose
    /// title comes from the picker's role.
    label: Option<String>,
    ref_sha: String,
    commits: Vec<CommitInfo>,
    selected: usize,
    query: String,
    filter_column: Option<CommitColumn>,
}

/// Modal listing a ref's first-parent history, fuzzy-filtered, with the
/// selected commit's diff previewed beside it.
///
/// Rows are newest-first as the walk produced them, and an empty query keeps
/// that order rather than sorting. Commit order is the information the list
/// carries.
///
/// Previews are lazy, one background [`ReviewSession`] build per sha, so
/// scrolling a long history does not diff every commit in it.
pub(crate) struct CommitPicker {
    pub(crate) role: CommitPickerRole,
    pub(crate) workdir: PathBuf,
    /// Full sha of the ref the picker was opened over. Excluded from
    /// [`Self::default_selection`], which looks for a branch other than the one
    /// the user is already looking at.
    pub(crate) ref_sha: String,
    pub(crate) input: InputView,
    /// Walked history, newest first.
    pub(crate) commits: Vec<CommitInfo>,
    /// Full sha to the local branch names pointing at it.
    pub(crate) branch_tips: HashMap<String, Vec<String>>,
    /// Indices into `commits`, in display order.
    pub(crate) filtered: Vec<usize>,
    /// Matched character offsets per filtered row, parallel to `filtered`.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    /// Rendered list height, refreshed each frame so [`Self::page`] can size
    /// its step. `None` before the first render.
    pub(crate) viewport_rows: Option<usize>,
    pub(crate) preview_sessions: HashMap<String, Arc<ReviewSession>>,
    pub(crate) pending_preview: Option<PendingPreview>,
    /// Sha the in-flight or most recent preview was requested for, so a build
    /// that lands after the selection moved on is discarded.
    pub(crate) requested_preview: Option<String>,
    /// Rows the diff preview is scrolled down by, for the selected commit.
    ///
    /// Belongs to whichever row is selected and drops when that changes, since
    /// a position in one commit's diff means nothing in another's. Clamped at
    /// render, where the diff's own row count is known.
    pub(crate) preview_scroll: usize,
    /// Column the query is scoped to, or `None` to match the whole row.
    ///
    /// Scoping is what makes the table searchable per field, so typing an
    /// author name stops hitting every title that happens to contain it.
    pub(crate) filter_column: Option<CommitColumn>,
    /// Unix epoch seconds the picker opened at, which every row's age column is
    /// measured against.
    ///
    /// Captured once rather than read per render, so the ages stay put while
    /// the user scrolls and a test can pin them to a fixed clock.
    pub(crate) now_epoch: i64,
    /// Scopes drilled through to reach the current list, oldest first.
    ///
    /// Empty at the root scope. Nesting is expected, since a merge inside a
    /// drilled branch drills again and each pop unwinds one level.
    #[allow(dead_code)]
    pub(crate) scope_stack: Vec<CommitScope>,
    /// What the current scope is called, or `None` at the root scope, where the
    /// title comes from the picker's role instead.
    pub(crate) scope_label: Option<String>,
}

impl CommitPicker {
    /// Build a picker over `commits`, starting on [`Self::default_selection`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        role: CommitPickerRole,
        workdir: PathBuf,
        ref_sha: String,
        commits: Vec<CommitInfo>,
        branch_tips: HashMap<String, Vec<String>>,
        now_epoch: i64,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::CommitPicker, "", "insert", 1);
        let mut picker = Self {
            now_epoch,
            role,
            workdir,
            ref_sha,
            input,
            commits,
            branch_tips,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview_sessions: HashMap::new(),
            pending_preview: None,
            requested_preview: None,
            filter_column: None,
            preview_scroll: 0,
            scope_stack: Vec::new(),
            scope_label: None,
        };
        picker.refilter("");
        picker.selected = picker.default_selection();
        picker
    }

    /// Drill into a new scope, parking the current one for [`Self::pop_scope`].
    ///
    /// `query_before` is the text in the input at the moment of the drill,
    /// which the picker cannot read itself. The caller supplies it and clears
    /// the input, because the new list arrives unfiltered.
    #[allow(dead_code)]
    pub(crate) fn push_scope(
        &mut self,
        label: String,
        ref_sha: String,
        commits: Vec<CommitInfo>,
        query_before: String,
    ) {
        self.scope_stack.push(CommitScope {
            label: self.scope_label.replace(label),
            ref_sha: mem::replace(&mut self.ref_sha, ref_sha),
            commits: mem::replace(&mut self.commits, commits),
            selected: self.selected,
            query: query_before,
            filter_column: self.filter_column.take(),
        });

        self.selected = 0;
        self.refilter("");
        self.preview_scroll = 0;
    }

    /// Pop back to the scope the last drill displaced, returning the query that
    /// was typed in it so the caller can restore the input.
    ///
    /// `None` at the root scope, where there is nothing to pop back to.
    #[allow(dead_code)]
    pub(crate) fn pop_scope(&mut self) -> Option<String> {
        let scope = self.scope_stack.pop()?;

        self.scope_label = scope.label;
        self.ref_sha = scope.ref_sha;
        self.commits = scope.commits;
        self.filter_column = scope.filter_column;
        self.selected = scope.selected;
        self.refilter(&scope.query);
        self.preview_scroll = 0;

        Some(scope.query)
    }

    /// The cells for `idx`, joined into the fuzzy haystack the filter matches.
    ///
    /// Every column is present even when empty, so a row's cells always line up
    /// with the table's columns. The join separates them with single spaces,
    /// which is what makes a match offset resolvable back to a cell.
    pub(crate) fn row(&self, idx: usize) -> CommitRow {
        let Some(commit) = self.commits.get(idx) else {
            return CommitRow {
                text: String::new(),
                cells: std::array::from_fn(|_| CommitCell {
                    text: String::new(),
                    start: 0,
                }),
            };
        };

        let branches = self
            .branch_tips
            .get(&commit.sha)
            .map(|names| names.join(" "))
            .unwrap_or_default();

        let texts = [
            commit.short_sha.clone(),
            branches,
            commit.summary.clone(),
            commit.author_name.clone(),
            age_label(self.now_epoch, commit.time),
        ];

        let mut text = String::new();
        let mut start = 0;
        let cells = texts.map(|cell| {
            if !text.is_empty() {
                text.push(' ');
                start += 1;
            }
            let cell_start = start;
            start += cell.chars().count();
            text.push_str(&cell);
            CommitCell {
                text: cell,
                start: cell_start,
            }
        });

        CommitRow { text, cells }
    }

    /// Re-rank the rows for `query`, matches first by score then by row text.
    /// An empty or whitespace-only query lists every commit newest-first with
    /// no highlights.
    pub(crate) fn refilter(&mut self, query: &str) {
        let column = self.filter_column;
        let rows: Vec<CommitRow> = (0..self.commits.len()).map(|idx| self.row(idx)).collect();

        // A scoped query searches one cell, so the matcher reports offsets into
        // that cell rather than into the join. Shifting them by the cell's own
        // start puts them back in join space, which is the only offset space the
        // renderer knows about.
        let shift = |idx: usize| match column {
            Some(column) => rows[idx].cells[column as usize].start as u32,
            None => 0,
        };
        let items: Vec<(usize, String)> = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let text = match column {
                    Some(column) => row.cells[column as usize].text.clone(),
                    None => row.text.clone(),
                };
                (idx, text)
            })
            .collect();

        let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
            self.filtered = (0..self.commits.len()).collect();
            self.match_indices = vec![Vec::new(); self.commits.len()];
            self.clamp_selected();
            return;
        };

        fuzzy::sort_ranked(&mut matches);

        self.filtered = Vec::with_capacity(matches.len());
        self.match_indices = Vec::with_capacity(matches.len());
        for m in matches {
            let shift = shift(m.item);
            self.match_indices
                .push(m.matched_indices.iter().map(|&i| i + shift).collect());
            self.filtered.push(m.item);
        }
        self.clamp_selected();
    }

    /// Advance which column the query searches, wrapping through every column
    /// and back to searching the whole row.
    ///
    /// Refilters as it goes, so the list narrows to the new column under the
    /// query already typed rather than waiting for the next keystroke.
    pub(crate) fn cycle_filter_column(&mut self, query: &str) {
        self.filter_column = match self.filter_column {
            None => Some(CommitColumn::Commit),
            Some(CommitColumn::Commit) => Some(CommitColumn::Branch),
            Some(CommitColumn::Branch) => Some(CommitColumn::Title),
            Some(CommitColumn::Title) => Some(CommitColumn::Author),
            Some(CommitColumn::Author) => Some(CommitColumn::Date),
            Some(CommitColumn::Date) => None,
        };
        self.refilter(query);
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        let mut next = self.selected;
        crate::picker::nav_move(self.filtered.len(), &mut next, delta);
        self.set_selected(next);
    }

    /// Move the cursor to `index`, dropping the preview scroll when the row it
    /// belongs to actually changes.
    ///
    /// Every path that moves the cursor goes through here, so the scroll cannot
    /// outlive the diff it was measured against. Gating on a real change is what
    /// keeps the per-keystroke clamp from resetting the scroll while the user is
    /// only typing.
    fn set_selected(&mut self, index: usize) {
        if self.selected != index {
            self.selected = index;
            self.preview_scroll = 0;
        }
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * crate::picker::nav_page_step(self.viewport_rows));
    }

    /// The commit under the selection cursor, or `None` for an empty list.
    pub(crate) fn selected_commit(&self) -> Option<&CommitInfo> {
        let idx = *self.filtered.get(self.selected)?;
        self.commits.get(idx)
    }

    /// The filtered row to start on.
    ///
    /// Under [`CommitPickerRole::PickBase`] this is the newest commit carrying
    /// a local branch other than the ref the picker was opened over, falling
    /// back to the newest row. Opening `:git-review main` from `main` most
    /// often means reviewing what another branch added, so the nearest such
    /// branch tip is a better landing spot than the tip the user is already
    /// sitting on.
    ///
    /// A browser starts at the newest row instead. That heuristic answers
    /// "where would a review begin", which is not a question a listing asks.
    pub(crate) fn default_selection(&self) -> usize {
        if self.role == CommitPickerRole::Browse {
            return 0;
        }
        self.filtered
            .iter()
            .position(|&idx| {
                let sha = &self.commits[idx].sha;
                *sha != self.ref_sha && self.branch_tips.contains_key(sha)
            })
            .unwrap_or(0)
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    fn clamp_selected(&mut self) {
        let mut next = self.selected;
        crate::picker::nav_clamp(self.filtered.len(), &mut next);
        self.set_selected(next);
    }
}

#[cfg(test)]
mod tests {
    use super::{age_label, CommitColumn, CommitPicker, CommitPickerRole, COMMIT_COLUMNS};
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        host::CommitInfo,
        input_view::{InputView, SubmitTarget},
    };
    use std::{collections::HashMap, path::PathBuf};

    fn commit(sha: &str, summary: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: summary.to_string(),
            author_name: "test".into(),
            author_email: "t@t".into(),
            time: 0,
            parent_count: 1,
        }
    }

    /// A picker over `commits` (newest first) with `branches` as
    /// `(name, sha)` pairs, opened over `ref_sha`.
    fn picker(commits: Vec<CommitInfo>, branches: &[(&str, &str)], ref_sha: &str) -> CommitPicker {
        let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
        for (name, sha) in branches {
            branch_tips
                .entry((*sha).to_string())
                .or_default()
                .push((*name).to_string());
        }
        let mut p = CommitPicker {
            role: CommitPickerRole::PickBase,
            workdir: PathBuf::from("/repo"),
            ref_sha: ref_sha.to_string(),
            input: InputView {
                editor_id: EditorId::default(),
                buffer_id: BufferId::new(0),
                target: SubmitTarget::CommitPicker,
                max_height: 1,
            },
            commits,
            branch_tips,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview_sessions: HashMap::new(),
            pending_preview: None,
            requested_preview: None,
            filter_column: None,
            preview_scroll: 0,
            // The fixture's commits sit at epoch zero, so this dates every row
            // to "now" and keeps the age column out of the row assertions.
            now_epoch: 0,
            scope_stack: Vec::new(),
            scope_label: None,
        };
        p.refilter("");
        p
    }

    fn history() -> Vec<CommitInfo> {
        vec![
            commit("ccc3333", "add the widget"),
            commit("bbb2222", "rename the gadget"),
            commit("aaa1111", "initial import"),
        ]
    }

    fn shown(p: &CommitPicker) -> Vec<String> {
        p.filtered
            .iter()
            .map(|&idx| p.commits[idx].sha.clone())
            .collect()
    }

    #[test]
    fn empty_query_keeps_newest_first_order() {
        let p = picker(history(), &[], "ccc3333");
        assert_eq!(shown(&p), ["ccc3333", "bbb2222", "aaa1111"]);
        assert_eq!(p.match_indices, vec![Vec::<u32>::new(); 3]);
    }

    #[test]
    fn refilter_matches_summary_text() {
        let mut p = picker(history(), &[], "ccc3333");
        p.refilter("widget");
        assert_eq!(shown(&p), ["ccc3333"]);
    }

    #[test]
    fn refilter_matches_an_abbreviated_sha() {
        let mut p = picker(history(), &[], "ccc3333");
        p.refilter("bbb2");
        assert_eq!(shown(&p), ["bbb2222"]);
    }

    #[test]
    fn refilter_matches_a_branch_name() {
        let mut p = picker(history(), &[("feature", "bbb2222")], "ccc3333");
        p.refilter("feature");
        assert_eq!(shown(&p), ["bbb2222"]);
    }

    #[test]
    fn row_cells_start_where_the_join_puts_them() {
        let p = picker(history(), &[("main", "ccc3333")], "ccc3333");
        let row = p.row(0);
        assert_eq!(row.text, "ccc3333 main add the widget test now");
        assert_eq!(
            row.cells.each_ref().map(|c| (c.text.as_str(), c.start)),
            [
                ("ccc3333", 0),
                ("main", 8),
                ("add the widget", 13),
                ("test", 28),
                ("now", 33),
            ],
            "every cell reports where its text begins in the join"
        );

        let bare = p.row(1);
        assert_eq!(
            bare.text, "bbb2222  rename the gadget test now",
            "a commit with no branch keeps the empty cell, so the columns line up"
        );
        assert_eq!(
            bare.cells[CommitColumn::Branch as usize].text,
            "",
            "and that cell is simply empty"
        );
        assert_eq!(
            bare.cells[CommitColumn::Title as usize].start,
            9,
            "the title still starts after the empty branch cell and its space"
        );
    }

    #[test]
    fn cycling_walks_every_column_and_returns_to_all() {
        let mut p = picker(history(), &[], "ccc3333");
        assert_eq!(p.filter_column, None, "an opened picker searches the row");

        let mut seen = Vec::new();
        for _ in 0..COMMIT_COLUMNS + 1 {
            p.cycle_filter_column("");
            seen.push(p.filter_column);
        }
        assert_eq!(
            seen,
            [
                Some(CommitColumn::Commit),
                Some(CommitColumn::Branch),
                Some(CommitColumn::Title),
                Some(CommitColumn::Author),
                Some(CommitColumn::Date),
                None,
            ],
            "one pass through the columns lands back on the whole row"
        );
    }

    /// The point of scoping is that a query stops hitting columns the user is
    /// not searching, so a summary word must miss while the sha is active.
    #[test]
    fn a_scoped_query_matches_only_its_own_column() {
        let mut p = picker(history(), &[("main", "ccc3333")], "ccc3333");
        p.refilter("widget");
        assert_eq!(shown(&p), ["ccc3333"], "unscoped, the summary matches");

        p.filter_column = Some(CommitColumn::Commit);
        p.refilter("widget");
        assert_eq!(
            shown(&p),
            Vec::<String>::new(),
            "scoped to the sha, a summary word matches nothing"
        );

        p.filter_column = Some(CommitColumn::Branch);
        p.refilter("main");
        assert_eq!(
            shown(&p),
            ["ccc3333"],
            "and the branch column finds its own text"
        );
    }

    /// Highlights are painted from offsets into the joined row, so a scoped
    /// match has to be shifted out of its cell or it would light up the sha.
    #[test]
    fn a_scoped_match_reports_offsets_into_the_joined_row() {
        let mut p = picker(history(), &[("main", "ccc3333")], "ccc3333");
        p.filter_column = Some(CommitColumn::Title);
        p.refilter("add");

        let row = p.row(p.filtered[0]);
        let title_start = row.cells[CommitColumn::Title as usize].start;
        assert_eq!(
            p.match_indices[0],
            [
                title_start as u32,
                title_start as u32 + 1,
                title_start as u32 + 2
            ],
            "the three matched characters sit where the title begins in the join"
        );
        assert!(
            row.text[..title_start].contains("ccc3333"),
            "which is past the sha, so the highlight cannot land on it"
        );
    }

    #[test]
    fn age_label_takes_the_coarsest_unit_it_fills() {
        let now = 1_000_000_000;
        let ago = |seconds| age_label(now, now - seconds);

        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now", "under a minute is not worth a number");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(3_600), "1h");
        assert_eq!(ago(86_400 * 2), "2d");
        assert_eq!(ago(2_592_000 * 3), "3mo");
        assert_eq!(ago(31_536_000 * 4), "4y");
    }

    /// A rewritten author date or a skewed clock can put a commit in the
    /// future, which must not read as a negative age.
    #[test]
    fn a_commit_dated_ahead_of_now_reads_as_now() {
        assert_eq!(age_label(1_000, 5_000), "now");
    }

    #[test]
    fn default_selection_picks_the_newest_other_branch_tip() {
        let p = picker(
            history(),
            &[("main", "ccc3333"), ("feature", "bbb2222")],
            "ccc3333",
        );
        assert_eq!(
            p.default_selection(),
            1,
            "the ref's own tip is skipped for the next branch down"
        );
    }

    #[test]
    fn default_selection_falls_back_to_the_newest_row() {
        assert_eq!(picker(history(), &[], "ccc3333").default_selection(), 0);
        assert_eq!(
            picker(history(), &[("main", "ccc3333")], "ccc3333").default_selection(),
            0,
            "a repo whose only branch is the ref itself has no better row"
        );
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut p = picker(history(), &[], "ccc3333");
        p.move_selection(-1);
        assert_eq!(p.selected, 0);
        p.move_selection(9);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn page_steps_by_half_the_viewport() {
        let mut p = picker(history(), &[], "ccc3333");
        p.viewport_rows = Some(4);
        p.page(1);
        assert_eq!(p.selected, 2);
        p.page(-1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn refilter_clamps_a_stale_selection() {
        let mut p = picker(history(), &[], "ccc3333");
        p.selected = 2;
        p.refilter("widget");
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn selected_commit_follows_the_cursor() {
        let mut p = picker(history(), &[], "ccc3333");
        p.move_selection(1);
        assert_eq!(p.selected_commit().map(|c| c.sha.as_str()), Some("bbb2222"));
    }

    fn branch_history() -> Vec<CommitInfo> {
        vec![
            commit("fff5555", "branch work"),
            commit("eee4444", "branch start"),
        ]
    }

    #[test]
    fn a_drilled_scope_installs_its_own_rows_at_the_top() {
        let mut p = picker(history(), &[], "ccc3333");
        p.filter_column = Some(CommitColumn::Title);
        p.move_selection(2);

        p.push_scope(
            "merge ccc3333".to_string(),
            "fff5555".to_string(),
            branch_history(),
            "gadget".to_string(),
        );

        assert_eq!(shown(&p), ["fff5555", "eee4444"], "the branch's own rows");
        assert_eq!(p.selected, 0, "a new scope starts at the top");
        assert_eq!(
            p.filter_column, None,
            "the column scope does not carry over"
        );
        assert_eq!(p.scope_label.as_deref(), Some("merge ccc3333"));
        assert_eq!(p.ref_sha, "fff5555");
    }

    #[test]
    fn popping_a_scope_restores_everything_the_drill_replaced() {
        let mut p = picker(history(), &[], "ccc3333");
        p.filter_column = Some(CommitColumn::Title);
        p.move_selection(2);

        p.push_scope(
            "merge ccc3333".to_string(),
            "fff5555".to_string(),
            branch_history(),
            "gadget".to_string(),
        );

        assert_eq!(p.pop_scope().as_deref(), Some("gadget"), "the typed query");
        assert_eq!(shown(&p), ["bbb2222"], "restored rows, refiltered by it");
        assert_eq!(p.selected, 0);
        assert_eq!(p.filter_column, Some(CommitColumn::Title));
        assert_eq!(p.scope_label, None, "back to the role-titled root");
        assert_eq!(p.ref_sha, "ccc3333");
    }

    #[test]
    fn nested_scopes_unwind_one_level_at_a_time() {
        let mut p = picker(history(), &[], "ccc3333");

        p.push_scope(
            "merge ccc3333".to_string(),
            "fff5555".to_string(),
            branch_history(),
            String::new(),
        );
        p.push_scope(
            "merge fff5555".to_string(),
            "ddd9999".to_string(),
            vec![commit("ddd9999", "inner work")],
            String::new(),
        );

        assert_eq!(shown(&p), ["ddd9999"]);
        assert_eq!(p.pop_scope(), Some(String::new()));
        assert_eq!(shown(&p), ["fff5555", "eee4444"], "one level back");
        assert_eq!(p.scope_label.as_deref(), Some("merge ccc3333"));

        assert_eq!(p.pop_scope(), Some(String::new()));
        assert_eq!(shown(&p), ["ccc3333", "bbb2222", "aaa1111"], "the root");
        assert_eq!(p.scope_label, None);
    }

    #[test]
    fn popping_the_root_scope_reports_nothing_to_pop() {
        let mut p = picker(history(), &[], "ccc3333");
        assert_eq!(p.pop_scope(), None);
        assert_eq!(
            shown(&p),
            ["ccc3333", "bbb2222", "aaa1111"],
            "list untouched"
        );
    }

    /// The `modal == commit_picker` keymap block reaches the handlers, over a
    /// picker opened the way a user opens one.
    ///
    /// Worth pinning because an unresolvable action name in config.stcfg is
    /// silently dropped, so a mistyped binding would leave the modal inert with
    /// nothing else to catch it. Opening through `:git-ls` rather than
    /// installing the picker by hand is what makes that real. The block is
    /// gated on insert mode, and only the true open path proves the picker
    /// lands there.
    #[test]
    fn the_keymap_block_drives_the_picker() {
        let mut h = seeded_repo_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();
        let selected = |h: &crate::test_harness::TestHarness| {
            h.stoat
                .commit_picker
                .as_ref()
                .and_then(|p| p.selected_commit())
                .map(|c| c.sha.to_string())
        };
        assert_eq!(
            selected(&h).as_deref(),
            Some("c3d4e5f6"),
            "the browser opens on the newest commit"
        );

        h.type_keys("down");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("b2c3d4e5"),
            "Down steps one commit back through history"
        );

        h.type_keys("ctrl-f");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("a1b2c3d4"),
            "Ctrl-f pages by half the viewport, which this short list clamps \
             to the oldest commit"
        );

        h.type_keys("ctrl-b");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("c3d4e5f6"),
            "and Ctrl-b pages back the same distance"
        );

        h.type_keys("escape");
        h.settle();
        assert!(h.stoat.commit_picker.is_none(), "Escape closes the picker");
    }

    /// The two roles share every row, so the title is the only thing telling
    /// the user whether picking a commit starts a review or just closes the
    /// listing.
    #[test]
    fn the_title_names_the_role_the_picker_was_opened_for() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        let pick_base = h.rendered_text();
        assert!(
            pick_base.contains(" review from commit "),
            "a base picker says what selecting a row will start:\n{pick_base}"
        );
        assert!(
            !pick_base.contains(" git log "),
            "and does not also claim to be a listing"
        );

        let mut h = seeded_repo_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();
        h.snapshot();
        let browse = h.rendered_text();
        assert!(
            browse.contains(" git log "),
            "a browser names itself for what it shows:\n{browse}"
        );
        assert!(
            !browse.contains(" review from commit "),
            "and never offers to start a review it will not start"
        );
    }

    /// Foreground of the header cell under `label`, on the row the labels are
    /// painted on.
    fn header_fg(h: &crate::test_harness::TestHarness, label: &str) -> ratatui::style::Color {
        let text = h.rendered_text();
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Commit") && line.contains("Branch"))
            .expect("the header row is painted");
        let x = line.find(label).expect("the label is painted") as u16;
        h.rendered_buffer()[(x, y as u16)].fg
    }

    /// Scoping is only usable if the table says which column it scoped to, so
    /// the active header has to look different from the rest.
    #[test]
    fn a_scoped_column_stands_out_from_the_dimmed_ones() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        assert_eq!(
            header_fg(&h, "Commit"),
            header_fg(&h, "Title"),
            "with no column scoped every header reads the same"
        );

        h.stoat
            .commit_picker
            .as_mut()
            .expect("picker open")
            .filter_column = Some(CommitColumn::Title);
        h.snapshot();
        assert_ne!(
            header_fg(&h, "Title"),
            header_fg(&h, "Commit"),
            "the scoped column's header separates from the others"
        );
        assert_eq!(
            header_fg(&h, "Commit"),
            header_fg(&h, "Author"),
            "and every column it is not scoped to still reads alike"
        );
    }

    fn wheel(h: &mut crate::test_harness::TestHarness, down: bool, col: u16, row: u16) {
        use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
        h.stoat.update(Event::Mouse(MouseEvent {
            kind: if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn layout(
        h: &crate::test_harness::TestHarness,
    ) -> crate::render::commit_picker::CommitPickerLayout {
        crate::render::commit_picker::commit_picker_layout(h.stoat.size(), 0)
            .expect("the picker fits the test terminal")
    }

    /// The picker covers the pane behind it, so a wheel anywhere over it has to
    /// act on the picker rather than falling through to that pane.
    #[test]
    fn a_wheel_over_the_list_walks_it_and_never_reaches_the_buffer() {
        let mut h = seeded_picker_harness();
        // Long enough that the buffer behind the modal has somewhere to scroll,
        // so its staying put is evidence rather than a clamp.
        h.seed_focused_buffer(&"line\n".repeat(200));
        h.snapshot();
        let scrolled = h.editor_scroll_rows();
        let list = layout(&h).list;
        let selected = h.stoat.commit_picker.as_ref().expect("open").selected;

        wheel(&mut h, true, list.x + 1, list.y + 1);

        assert_eq!(
            h.stoat.commit_picker.as_ref().expect("open").selected,
            selected + 1,
            "a notch over the rows steps the selection one commit"
        );
        assert_eq!(
            h.editor_scroll_rows(),
            scrolled,
            "and never reaches the editor the modal is covering"
        );
    }

    #[test]
    fn a_wheel_over_the_diff_scrolls_it_and_leaves_the_selection_put() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        let preview = layout(&h).preview.expect("the diff pane is present");
        let selected = h.stoat.commit_picker.as_ref().expect("open").selected;

        wheel(&mut h, true, preview.x + 1, preview.y + 1);

        let picker = h.stoat.commit_picker.as_ref().expect("open");
        assert_eq!(picker.selected, selected, "the list stays where it was");
        assert!(
            picker.preview_scroll > 0,
            "and the diff scrolls down instead"
        );
    }

    /// A position in one commit's diff means nothing in another's, so moving the
    /// selection has to drop it.
    #[test]
    fn moving_the_selection_drops_the_diff_scroll() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        let preview = layout(&h).preview.expect("the diff pane is present");
        wheel(&mut h, true, preview.x + 1, preview.y + 1);
        assert!(h.stoat.commit_picker.as_ref().expect("open").preview_scroll > 0);

        let list = layout(&h).list;
        wheel(&mut h, true, list.x + 1, list.y + 1);

        assert_eq!(
            h.stoat.commit_picker.as_ref().expect("open").preview_scroll,
            0,
            "the scroll does not survive onto a different commit's diff"
        );
    }

    #[test]
    fn snapshot_commit_picker_open() {
        let mut h = seeded_picker_harness();
        h.assert_snapshot("commit_picker_open");
    }

    /// Clock the seeded picker measures its ages against, three days and change
    /// past the commits the fake repo dates to. Fixed so the age column reads
    /// the same on every run rather than drifting with the wall clock, and off
    /// the day boundary so commits seconds apart do not straddle a bucket.
    const SEEDED_NOW_EPOCH: i64 = 1_700_000_000 + 3 * 86_400 + 3_600;

    /// A harness with a three-commit `/repo`, `main` on its tip and `feature`
    /// one commit back, and that repo as the workspace root so the git actions
    /// find it.
    fn seeded_repo_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::app::Stoat::test();
        h.resize(100, 24);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "b2c3d4e5",
                    "chore: tweak a",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n")],
                ),
                (
                    "c3d4e5f6",
                    "feat: add b.rs",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n"), ("b.rs", "fn b() {}\n")],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "c3d4e5f6")
            .branch("feature", "b2c3d4e5")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    /// [`seeded_repo_harness`] with a PickBase picker installed over that
    /// history, which is the role a review walk starts from and `:git-ls`
    /// cannot produce.
    fn seeded_picker_harness() -> crate::test_harness::TestHarness {
        let mut h = seeded_repo_harness();

        let (commits, branch_tips) = {
            let repo = h
                .stoat
                .git_host
                .discover(std::path::Path::new("/repo"))
                .expect("seeded repo");
            let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
            for (name, sha) in repo.local_branches() {
                branch_tips.entry(sha).or_default().push(name);
            }
            (repo.log_from("c3d4e5f6", 100), branch_tips)
        };

        let executor = h.stoat.executor.clone();
        let picker = CommitPicker::new(
            h.stoat.active_workspace_mut(),
            executor,
            CommitPickerRole::PickBase,
            PathBuf::from("/repo"),
            "c3d4e5f6".to_string(),
            commits,
            branch_tips,
            SEEDED_NOW_EPOCH,
        );
        h.stoat.commit_picker = Some(picker);
        h.settle();
        h
    }
}
