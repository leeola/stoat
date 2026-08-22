use crate::{
    commit_graph::{self, GraphRow},
    commit_list::{PendingPreview, PreviewCache},
    fuzzy,
    host::CommitInfo,
    input_view::{InputView, SubmitTarget},
    picker,
    workspace::Workspace,
};
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    path::PathBuf,
};
use stoat_scheduler::{Executor, Task};

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
#[derive(Clone)]
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
#[derive(Clone)]
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

/// Longest text in each column across the rows `filtered` names, in
/// characters.
///
/// `rows` is indexed by commit, not by filter position, so this reads through
/// `filtered` rather than zipping the two.
fn measure_columns(rows: &[CommitRow], filtered: &[usize]) -> [u16; COMMIT_COLUMNS] {
    let mut widest = [0u16; COMMIT_COLUMNS];
    for &idx in filtered {
        let Some(row) = rows.get(idx) else { continue };
        for (column, cell) in row.cells.iter().enumerate() {
            widest[column] = widest[column].max(cell.text.chars().count() as u16);
        }
    }
    widest
}

/// A picker scope a drill-in displaced, kept so popping back restores exactly
/// what the user was looking at.
///
/// Drilling into a merge replaces the whole list, and the user expects Alt-Left
/// to undo that completely rather than dropping them at the top of the old list
/// with their query gone. Everything a drill overwrites is saved here.
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

/// A finished history walk, and which list it is.
///
/// The two differ in what has to be installed alongside the commits, and both
/// sets of extras are known when the walk is spawned, so they travel with it
/// rather than being parked separately and paired up on arrival.
pub(crate) enum LoadedCommits {
    /// The list the picker opened over, with the branch names to label its tips.
    Root {
        commits: Vec<CommitInfo>,
        branch_tips: HashMap<String, Vec<String>>,
    },
    /// A merge drilled into, carrying what [`CommitPicker::push_scope`] needs to
    /// park the scope being left.
    Scope {
        label: String,
        ref_sha: String,
        commits: Vec<CommitInfo>,
        query_before: String,
    },
}

/// Modal listing a ref's first-parent history, fuzzy-filtered, with the
/// selected commit's diff previewed beside it.
///
/// Rows are newest-first as the walk produced them, and an empty query keeps
/// that order rather than sorting. Commit order is the information the list
/// carries.
///
/// Previews are lazy, one background [`DiffDocument`] build per sha, so
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
    /// The parse behind the current ranking, so a painted row past the indexed
    /// block derives its offsets rather than going unhighlighted.
    last_pattern: Option<fuzzy::Pattern>,
    pub(crate) selected: usize,
    /// Rendered list height, refreshed each frame so [`Self::page`] can size
    /// its step. `None` before the first render.
    pub(crate) viewport_rows: Option<usize>,
    pub(crate) preview_sessions: PreviewCache,
    pub(crate) pending_preview: Option<PendingPreview>,
    /// History walk running on a worker, waiting to be installed.
    ///
    /// Walking a thousand commits reads and decodes each one under the repo
    /// lock, which is too long to spend in the keystroke that asked for it, so
    /// the picker opens on nothing and fills in when the walk lands.
    pub(crate) pending_commits: Option<Task<LoadedCommits>>,
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
    pub(crate) scope_stack: Vec<CommitScope>,
    /// What the current scope is called, or `None` at the root scope, where the
    /// title comes from the picker's role instead.
    pub(crate) scope_label: Option<String>,
    /// Lane layout over [`Self::commits`], with the lane count the widest row
    /// needs, computed once per scope.
    ///
    /// Indexes `commits` rather than `filtered`, so filtering never invalidates
    /// it. That is also why the graph column only shows while the query is
    /// empty. A filtered list is non-contiguous, and edges drawn between rows
    /// that are no longer adjacent would claim a history that does not exist.
    pub(crate) graph: (Vec<GraphRow>, u16),
    /// Lanes the graph column should draw, or `None` while it hides.
    ///
    /// The graph is laid out over [`Self::commits`], so it only lines up while
    /// the visible list is that same sequence in that same order. A fuzzy filter
    /// both drops and reorders rows, and edges drawn across the survivors would
    /// claim an adjacency the history does not have, so the column collapses
    /// rather than lie.
    ///
    /// Stored rather than derived on demand, because deciding it means walking
    /// the whole filtered list and the render asks four times a frame.
    ///
    /// [`Self::refilter`] maintains it, which covers every way the answer can
    /// move. Nothing else writes [`Self::filtered`], and each assignment to
    /// [`Self::graph`] is followed by a refilter.
    pub(crate) graph_lanes: Option<u16>,
    /// Longest text in each column across every filtered row, in characters.
    ///
    /// Measured over the whole filtered list rather than the rows on screen,
    /// for two reasons. Columns sized to the visible slice resize under the
    /// user as a wider cell scrolls into view, and a pooled page painting an
    /// arbitrary span has to land on the same geometry every other page did.
    pub(crate) col_widest: [u16; COMMIT_COLUMNS],
    /// Bumped by every [`Self::refilter`], so a pool can tell that the rows it
    /// buffered are stale without hashing the whole filtered list.
    pub(crate) filter_generation: u64,
    /// One built [`CommitRow`] per entry in [`Self::commits`], in the same
    /// order.
    ///
    /// Building a row joins its branch names, formats an age against
    /// [`Self::now_epoch`], and assembles the haystack, none of which changes
    /// while the list stands. The filter runs every drive tick, so deriving
    /// them per run would rebuild the whole list between keystrokes.
    rows: Vec<CommitRow>,
    /// Bumped whenever [`Self::commits`] is replaced, so the filter can tell a
    /// new list from a re-run over the same one.
    commits_generation: u64,
    /// Hash of the inputs the last [`Self::refilter`] ran against, or `None`
    /// before the first.
    last_filter_key: Option<u64>,
}

impl CommitPicker {
    /// Build a picker holding no history yet.
    ///
    /// The walk that fills it runs on a worker and arrives through
    /// [`Self::set_commits`]. Building the rows, lanes and filter over the empty
    /// list here rather than only on arrival means the picker is a consistent
    /// one from the moment it exists, so it renders and takes input while the
    /// walk is still running.
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        role: CommitPickerRole,
        workdir: PathBuf,
        ref_sha: String,
        now_epoch: i64,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::CommitPicker, "", "insert", 1);
        let mut picker = Self {
            now_epoch,
            role,
            workdir,
            ref_sha,
            input,
            commits: Vec::new(),
            branch_tips: HashMap::new(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            last_pattern: None,
            selected: 0,
            viewport_rows: None,
            preview_sessions: PreviewCache::default(),
            pending_preview: None,
            pending_commits: None,
            requested_preview: None,
            filter_column: None,
            preview_scroll: 0,
            scope_stack: Vec::new(),
            scope_label: None,
            graph: (Vec::new(), 0),
            graph_lanes: None,
            col_widest: [0; COMMIT_COLUMNS],
            filter_generation: 0,
            rows: Vec::new(),
            commits_generation: 0,
            last_filter_key: None,
        };
        picker.rebuild_rows();
        picker.graph = commit_graph::assign_lanes(&picker.commits);
        picker.refilter("");
        picker.selected = picker.default_selection();
        picker
    }

    /// Install the walk the picker opened over, filtered by whatever `query` has
    /// been typed while it ran.
    ///
    /// The query is the caller's to supply, as in [`Self::push_scope`], because
    /// the input's text lives in the workspace rather than on the picker. Losing
    /// it here would discard what the user typed against a list that had not
    /// arrived yet.
    pub(crate) fn set_commits(
        &mut self,
        commits: Vec<CommitInfo>,
        branch_tips: HashMap<String, Vec<String>>,
        query: &str,
    ) {
        self.commits = commits;
        self.branch_tips = branch_tips;
        self.rebuild_rows();
        self.graph = commit_graph::assign_lanes(&self.commits);
        self.refilter(query);
        self.selected = self.default_selection();
    }

    /// Drill into a new scope, parking the current one for [`Self::pop_scope`].
    ///
    /// `query_before` is the text in the input at the moment of the drill,
    /// which the picker cannot read itself. The caller supplies it and clears
    /// the input, because the new list arrives unfiltered.
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
        self.rebuild_rows();
        self.graph = commit_graph::assign_lanes(&self.commits);
        self.refilter("");
        self.preview_scroll = 0;
    }

    /// Pop back to the scope the last drill displaced, returning the query that
    /// was typed in it so the caller can restore the input.
    ///
    /// `None` at the root scope, where there is nothing to pop back to.
    pub(crate) fn pop_scope(&mut self) -> Option<String> {
        let scope = self.scope_stack.pop()?;

        self.scope_label = scope.label;
        self.ref_sha = scope.ref_sha;
        self.commits = scope.commits;
        self.filter_column = scope.filter_column;
        self.selected = scope.selected;
        self.rebuild_rows();
        self.graph = commit_graph::assign_lanes(&self.commits);
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
        self.rows.get(idx).cloned().unwrap_or_else(|| CommitRow {
            text: String::new(),
            cells: std::array::from_fn(|_| CommitCell {
                text: String::new(),
                start: 0,
            }),
        })
    }

    /// Rebuild every row from the current commits and invalidate the filter.
    ///
    /// Must run wherever [`Self::commits`] is replaced. The generation bump is
    /// what tells [`Self::refilter`] that an otherwise identical query is now
    /// being asked of a different list.
    fn rebuild_rows(&mut self) {
        self.rows = self
            .commits
            .iter()
            .map(|commit| self.build_row(commit))
            .collect();
        self.commits_generation = self.commits_generation.wrapping_add(1);
    }

    fn build_row(&self, commit: &CommitInfo) -> CommitRow {
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

        // The result is a pure function of these three, so an unchanged key
        // means an identical outcome. The filter runs every drive tick, and
        // bumping the generation is what makes a pool discard the pages it
        // buffered, so an idle picker would otherwise refill them all forever.
        let key = {
            let mut hasher = DefaultHasher::new();
            query.hash(&mut hasher);
            column.map(|column| column as usize).hash(&mut hasher);
            self.commits_generation.hash(&mut hasher);
            hasher.finish()
        };
        if self.last_filter_key == Some(key) {
            return;
        }
        self.last_filter_key = Some(key);

        let items = self.rows.iter().enumerate().map(|(idx, row)| {
            let text = match column {
                Some(column) => row.cells[column as usize].text.as_str(),
                None => row.text.as_str(),
            };
            (idx, text)
        });
        self.last_pattern = picker::rank_into(
            query,
            items,
            self.commits.len(),
            &mut self.filtered,
            &mut self.match_indices,
        );

        // A scoped query searches one cell, so the matcher reports offsets into
        // that cell rather than into the join. Shifting them by the cell's own
        // start puts them back in join space, which is the only offset space the
        // renderer knows about.
        if let Some(column) = column {
            for (&row, indices) in self.filtered.iter().zip(self.match_indices.iter_mut()) {
                let shift = self.rows[row].cells[column as usize].start as u32;
                for index in indices.iter_mut() {
                    *index += shift;
                }
            }
        }

        self.col_widest = measure_columns(&self.rows, &self.filtered);
        self.filter_generation = self.filter_generation.wrapping_add(1);

        let unfiltered = self.filtered.iter().copied().eq(0..self.commits.len());
        self.graph_lanes = unfiltered.then_some(self.graph.1);

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
        picker::nav_move(self.filtered.len(), &mut next, delta);
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
        self.move_selection(dir * picker::nav_page_step(self.viewport_rows));
    }

    /// Jump the selection to the nearest branch tip in `dir` (negative up the
    /// list toward newer commits, positive down toward older ones).
    ///
    /// A tip is the row a reader orients by, so this skips the ordinary commits
    /// between them rather than counting rows with [`Self::move_selection`]. Any
    /// local branch counts, the picker's own ref included. What matters is that
    /// the row is a branch head, not which head it is.
    ///
    /// The scan runs over the visible rows, so a tip the query filtered out is
    /// not somewhere to jump to. It does not wrap, and a direction with no tip
    /// left in it leaves the selection alone.
    pub(crate) fn select_branch(&mut self, dir: i32) {
        let is_tip = |&row: &usize| {
            self.commits
                .get(self.filtered[row])
                .is_some_and(|commit| self.branch_tips.contains_key(&commit.sha))
        };
        let found = match dir >= 0 {
            true => (self.selected + 1..self.filtered.len()).find(is_tip),
            false => (0..self.selected).rev().find(is_tip),
        };
        if let Some(row) = found {
            self.set_selected(row);
        }
    }

    /// Matched offsets to highlight in filtered `row`, in the joined row's
    /// offset space.
    ///
    /// This does not reach [`picker::row_indices`] because a scoped query
    /// matched one cell, and offsets derived here arrive in that cell's space.
    /// Shifting them by the cell's own start is what the refilter already did
    /// to the rows it stored.
    ///
    /// `scratch` holds a derived list and `matching` is working memory, both
    /// reused across the rows a caller paints.
    pub(crate) fn row_indices<'a>(
        &'a self,
        row: usize,
        scratch: &'a mut Vec<u32>,
        matching: &mut fuzzy::Scratch,
    ) -> &'a [u32] {
        if let Some(indices) = self.match_indices.get(row) {
            return indices;
        }

        scratch.clear();
        let (Some(&idx), Some(pattern)) = (self.filtered.get(row), self.last_pattern.as_ref())
        else {
            return scratch;
        };
        let (haystack, shift) = match self.filter_column {
            Some(column) => {
                let cell = &self.rows[idx].cells[column as usize];
                (cell.text.as_str(), cell.start as u32)
            },
            None => (self.rows[idx].text.as_str(), 0),
        };

        fuzzy::indices_of_parsed(pattern, haystack, scratch, matching);
        for index in scratch.iter_mut() {
            *index += shift;
        }
        scratch
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

    /// The rows the table would paint starting at `start_row`, at most `rows`
    /// of them.
    ///
    /// The window is the caller's to choose. A live picker follows its
    /// selection, while a pooled page covers a fixed span of the list.
    pub(crate) fn window(&self, start_row: usize, rows: usize) -> Vec<CommitRow> {
        self.filtered
            .iter()
            .skip(start_row)
            .take(rows)
            .map(|&idx| self.row(idx))
            .collect()
    }

    fn clamp_selected(&mut self) {
        let mut next = self.selected;
        picker::nav_clamp(self.filtered.len(), &mut next);
        self.set_selected(next);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        age_label, CommitColumn, CommitPicker, CommitPickerRole, PreviewCache, COMMIT_COLUMNS,
    };
    use crate::{
        apc_emit,
        app::{ModalKind, MIN_PREVIEW_ROWS},
        buffer::BufferId,
        commit_graph,
        commit_list::Preview,
        editor_state::EditorId,
        host::CommitInfo,
        input_view::{InputView, SubmitTarget},
        render::commit_picker::MIN_LIST_ROWS,
    };
    use crossterm::event::{MouseButton, MouseEventKind};
    use std::{collections::HashMap, path::PathBuf};
    use stoatty_protocol::command::{self, PolylineCommand};

    fn commit(sha: &str, summary: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: summary.to_string(),
            author_name: "test".into(),
            author_email: "t@t".into(),
            time: 0,
            parents: Vec::new(),
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
            last_pattern: None,
            selected: 0,
            viewport_rows: None,
            preview_sessions: PreviewCache::default(),
            pending_preview: None,
            pending_commits: None,
            requested_preview: None,
            filter_column: None,
            preview_scroll: 0,
            // The fixture's commits sit at epoch zero, so this dates every row
            // to "now" and keeps the age column out of the row assertions.
            now_epoch: 0,
            scope_stack: Vec::new(),
            scope_label: None,
            graph: (Vec::new(), 0),
            graph_lanes: None,
            col_widest: [0; COMMIT_COLUMNS],
            filter_generation: 0,
            rows: Vec::new(),
            commits_generation: 0,
            last_filter_key: None,
        };
        p.rebuild_rows();
        p.graph = commit_graph::assign_lanes(&p.commits);
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

    /// A history whose newest commit merges a side branch back into the
    /// mainline, so the graph has a two-parent row to draw.
    fn merge_history() -> Vec<CommitInfo> {
        let mut merge = commit("mmm0000", "merge the feature");
        merge.parents = vec!["bbb2222".into(), "fff4444".into()];
        let mut mainline = commit("bbb2222", "rename the gadget");
        mainline.parents = vec!["aaa1111".into()];
        let mut feature = commit("fff4444", "add the widget");
        feature.parents = vec!["aaa1111".into()];

        vec![
            merge,
            mainline,
            feature,
            commit("aaa1111", "initial import"),
        ]
    }

    /// A theme stating both the RGB foreground the graph's rich path gates on
    /// and the modal background its blends and halos are drawn from, rather
    /// than relying on whatever the default carries.
    fn graph_theme() -> crate::theme::Theme {
        let src = r##"theme t { ui.text.fg = "#ffffff"; ui.modal.palette.bg = "#282c34"; }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The modal background [`graph_theme`] declares, which is the color every
    /// halo and merge hole is drawn in.
    const GRAPH_BG: [u8; 3] = [0x28, 0x2c, 0x34];

    /// Every node disc the graph emitted, in emission order, as
    /// `(width, color)`. A node is the zero-length path the renderer draws as a
    /// disc, so the lane runs between rows -- which carry two or more points --
    /// are left out.
    fn node_discs(scene: &mut stoat_widgets::ApcScene) -> Vec<(u16, [u8; 3])> {
        use stoatty_protocol::command::Command;

        command::decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Polyline(line) if line.points.len() == 1 => Some((line.width, line.color)),
                _ => None,
            })
            .collect()
    }

    /// Every lane transition the graph emitted, in emission order. A transition
    /// is the only path whose ends sit in different columns, so straight runs
    /// and node discs fall out.
    fn lane_transitions(scene: &mut stoat_widgets::ApcScene) -> Vec<PolylineCommand> {
        use stoatty_protocol::command::Command;

        command::decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Polyline(line)
                    if line.points.first().map(|p| p[0]) != line.points.last().map(|p| p[0]) =>
                {
                    Some(line)
                },
                _ => None,
            })
            .collect()
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

    /// A five-commit history whose only branch tips are the middle row and the
    /// oldest, so a jump has to pass over ordinary commits to reach one.
    fn tipped_history() -> Vec<CommitInfo> {
        vec![
            commit("eee5555", "newest"),
            commit("ddd4444", "fourth"),
            commit("ccc3333", "middle"),
            commit("bbb2222", "second"),
            commit("aaa1111", "oldest"),
        ]
    }

    #[test]
    fn a_branch_jump_skips_the_commits_between_tips() {
        let mut p = picker(
            tipped_history(),
            &[("feature", "ccc3333"), ("root", "aaa1111")],
            "eee5555",
        );

        p.select_branch(1);
        assert_eq!(p.selected, 2, "down lands on the first tip below row 0");
        p.select_branch(1);
        assert_eq!(p.selected, 4, "and again on the next one past it");
        p.select_branch(-1);
        assert_eq!(p.selected, 2, "up walks back to the tip above");
    }

    #[test]
    fn a_branch_jump_stops_rather_than_wrapping() {
        let mut p = picker(tipped_history(), &[("root", "aaa1111")], "eee5555");

        p.select_branch(-1);
        assert_eq!(p.selected, 0, "no tip lies above row 0, so nothing moves");

        p.select_branch(1);
        assert_eq!(p.selected, 4, "down reaches the only tip");
        p.select_branch(1);
        assert_eq!(
            p.selected, 4,
            "and stays there rather than wrapping to the top"
        );
    }

    /// The scan runs over the visible rows, so a tip the query filtered out is
    /// not somewhere the jump can land.
    #[test]
    fn a_branch_jump_ignores_a_tip_the_filter_hid() {
        let mut p = picker(
            tipped_history(),
            &[("feature", "ccc3333"), ("root", "aaa1111")],
            "eee5555",
        );
        p.refilter("newest");
        assert_eq!(shown(&p), ["eee5555"], "only the non-tip row survives");

        p.select_branch(1);
        assert_eq!(p.selected, 0, "with no visible tip below, nothing moves");
    }

    #[test]
    fn a_history_without_branches_never_jumps() {
        let mut p = picker(tipped_history(), &[], "eee5555");
        p.select_branch(1);
        p.select_branch(-1);
        assert_eq!(p.selected, 0, "no tips means no rows to jump to");
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

    /// The filter runs every drive tick, so an idle picker must leave the
    /// generation alone. A bump there tells the pool its buffered pages are
    /// stale, and a per-tick bump refills every page while nothing changes.
    #[test]
    fn refiltering_the_same_inputs_leaves_the_generation_alone() {
        let mut p = picker(history(), &[], "ccc3333");
        let settled = p.filter_generation;

        for _ in 0..5 {
            p.refilter("widget");
        }
        let after_query = p.filter_generation;
        assert_ne!(
            after_query, settled,
            "the first run with a new query filters"
        );

        for _ in 0..5 {
            p.refilter("widget");
        }
        assert_eq!(
            p.filter_generation, after_query,
            "re-running an unchanged query changes nothing",
        );

        p.refilter("gadget");
        assert_ne!(
            p.filter_generation, after_query,
            "a changed query filters again",
        );
    }

    /// The gate keys on the commit generation, so a scope change has to refilter
    /// even when the query it is asked for is the one already cached.
    #[test]
    fn a_scope_change_refilters_under_an_unchanged_query() {
        let mut p = picker(history(), &[], "ccc3333");
        p.refilter("");
        let before = p.filter_generation;

        p.push_scope(
            "merge ccc3333".to_string(),
            "fff5555".to_string(),
            branch_history(),
            String::new(),
        );

        assert_ne!(
            p.filter_generation, before,
            "the new list refilters despite the query staying empty",
        );
        assert_eq!(
            shown(&p),
            ["fff5555", "eee4444"],
            "the filter ran against the scope's own rows",
        );
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
        assert_eq!(
            p.graph_lanes, None,
            "and the restored filter hides the graph, over the drilled scope's lanes"
        );
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

    /// The seeded history puts `main` on the newest commit and `feature` one
    /// back, leaving the oldest with no branch, so the jumps have a tip to reach
    /// in one direction and nothing to reach in the other.
    #[test]
    fn the_keymap_block_jumps_between_branch_tips() {
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

        h.type_keys("ctrl-down");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("b2c3d4e5"),
            "Ctrl-Down jumps down the list to feature's tip"
        );

        h.type_keys("ctrl-down");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("b2c3d4e5"),
            "the oldest commit carries no branch, so there is nowhere further \
             down to jump"
        );

        h.type_keys("ctrl-up");
        h.settle();
        assert_eq!(
            selected(&h).as_deref(),
            Some("c3d4e5f6"),
            "and Ctrl-Up jumps back up to main's tip"
        );
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

    fn press(h: &mut crate::test_harness::TestHarness, kind: MouseEventKind, col: u16, row: u16) {
        use crossterm::event::{Event, KeyModifiers, MouseEvent};
        h.stoat.update(Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn layout(
        h: &crate::test_harness::TestHarness,
    ) -> crate::render::commit_picker::CommitPickerLayout {
        let lanes = h.stoat.commit_picker.as_ref().and_then(|p| p.graph_lanes);
        crate::render::commit_picker::commit_picker_layout(
            h.stoat.size(),
            lanes,
            0,
            crate::app::modal_split_percent(&h.stoat.modal_split, ModalKind::CommitPicker),
        )
        .expect("the picker fits the test terminal")
    }

    /// Paint `p`'s graph into a fresh buffer and scene under `theme`, returning
    /// both so a test can look at either path's output.
    ///
    /// `live` is whether the host can draw APC components, the other half of the
    /// rich-versus-glyph decision alongside the theme's colors.
    fn painted_graph(
        p: &CommitPicker,
        theme: &crate::theme::Theme,
        live: bool,
    ) -> (ratatui::buffer::Buffer, stoat_widgets::ApcScene) {
        let area = ratatui::layout::Rect::new(0, 0, 8, 4);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(live);
        crate::render::commit_picker::paint_commit_graph(p, 0, area, theme, &mut buf, &mut scene);
        (buf, scene)
    }

    /// Every APC command emitted since the last drain.
    fn drained_apc(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Vec<command::Command> {
        let mut cmds = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            cmds.extend(command::decode_stream(&batch));
        }
        cmds
    }

    /// The picker's selected commit with its diff built, so the preview has
    /// something to pool. Panics if the background build never lands.
    fn harness_with_preview() -> crate::test_harness::TestHarness {
        let mut h = seeded_picker_harness();
        h.settle();
        h.snapshot();
        h.settle();
        let picker = h.stoat.commit_picker.as_ref().expect("open");
        let sha = picker
            .selected_commit()
            .expect("a row is selected")
            .sha
            .clone();
        assert!(
            matches!(picker.preview_sessions.get(&sha), Preview::Built(_)),
            "the selected commit's diff builds during settle"
        );
        h
    }

    #[test]
    fn the_diff_preview_is_pooled_and_retired() {
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = harness_with_preview();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let preview = layout(&h).preview.expect("the diff pane is present");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_PREVIEW,
            top: preview.y,
            left: preview.x,
            width: preview.width,
            height: preview.height,
            window: 0,
        };
        assert!(
            drained_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the diff declares a pool at the preview rect"
        );

        h.stoat.commit_picker = None;
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        assert!(
            drained_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_PREVIEW,
            })),
            "closing the picker retires it"
        );
    }

    /// A page and the live painter show the same diff rows for the same offset,
    /// so a glide shows no shift as the pool composites over them.
    #[test]
    fn a_preview_page_paints_what_the_live_skip_shows() {
        let h = harness_with_preview();
        let preview = layout(&h).preview.expect("the diff pane is present");
        let picker = h.stoat.commit_picker.as_ref().expect("open");
        let sha = picker
            .selected_commit()
            .expect("a row is selected")
            .sha
            .clone();
        let Preview::Built(session) = picker.preview_sessions.get(&sha) else {
            panic!("the diff was built above");
        };

        let area = ratatui::layout::Rect::new(0, 0, preview.width, preview.height);
        for page in [0u64, 1] {
            let pooled = crate::smooth_scroll::render_commit_picker_preview_page(
                session,
                page,
                &h.stoat.theme,
                preview.width,
                preview.height,
            );

            let mut live = crate::smooth_scroll::page_buffer(area, &h.stoat.theme);
            let mut scene = stoat_widgets::ApcScene::new();
            crate::render::commits::render_commit_preview(
                session,
                &h.stoat.theme,
                area,
                page as usize * preview.height as usize,
                &mut live,
                &mut scene,
            );
            let mut expected = crate::render::serialize_buffer(&live);
            expected.extend_from_slice(scene.buffer());

            assert_eq!(pooled, expected, "page {page} matches the live skip");
        }
    }

    #[test]
    fn the_commit_table_is_pooled_and_retired() {
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = seeded_picker_harness();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let body = layout(&h).body();
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_LIST,
            top: body.y,
            left: body.x,
            width: body.width,
            height: body.height,
            window: 0,
        };
        assert!(
            drained_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the picker declares a pool spanning its graph column and table"
        );

        h.stoat.commit_picker = None;
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        assert!(
            drained_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_LIST,
            })),
            "closing the picker retires its pool"
        );
    }

    /// A pooled page and the live painter cover the same rows with the same
    /// geometry, so a glide shows no shift as the pool composites over them.
    #[test]
    fn page_zero_paints_what_the_live_table_paints() {
        let h = seeded_picker_harness();
        let picker = h.stoat.commit_picker.as_ref().expect("open");
        let body = layout(&h).body();
        let lanes = picker.graph_lanes;

        let pooled = crate::smooth_scroll::render_commit_picker_list_page(
            picker,
            lanes,
            0,
            &h.stoat.theme,
            body.width,
            body.height,
        );

        let mut live = crate::smooth_scroll::page_buffer(
            ratatui::layout::Rect::new(0, 0, body.width, body.height),
            &h.stoat.theme,
        );
        let graph_cells = lanes
            .map(crate::render::commit_picker::graph_width)
            .unwrap_or(0);
        let table =
            ratatui::layout::Rect::new(graph_cells, 0, body.width - graph_cells, body.height);
        crate::render::commit_picker::paint_commit_picker_rows(
            picker,
            0,
            ratatui::layout::Rect::new(table.x, 0, table.width, 0),
            table,
            &h.stoat.theme,
            &mut live,
        );
        let live_bytes = crate::render::serialize_buffer(&live);

        assert!(
            pooled.starts_with(&live_bytes),
            "the page's cells match the live painter's before its graph frames"
        );
    }

    #[test]
    fn a_column_sizes_to_the_whole_list_not_the_visible_rows() {
        let mut long = history();
        long.push(commit(
            "ddd4444",
            "a summary long enough to widen its column",
        ));
        let p = picker(long, &[], "ccc3333");

        let title = p.col_widest[CommitColumn::Title as usize];
        assert_eq!(
            title, 41,
            "the widest title sizes the column even though it is the last row"
        );
        assert!(
            p.window(0, 2)
                .iter()
                .all(|row| row.cells[CommitColumn::Title as usize].text.len() < title as usize),
            "and no row in the first window is that wide, so the measure is not local"
        );
    }

    #[test]
    fn the_graph_hides_as_soon_as_a_filter_reorders_the_list() {
        let mut p = picker(history(), &[], "ccc3333");
        assert_eq!(p.graph_lanes, Some(1), "a linear history lays out one lane");

        p.refilter("gadget");
        assert_eq!(
            p.graph_lanes, None,
            "a filtered list is no longer the sequence the graph indexes"
        );

        p.refilter("");
        assert_eq!(p.graph_lanes, Some(1), "clearing the query restores it");
    }

    #[test]
    fn the_graph_column_shifts_the_table_right() {
        let h = seeded_picker_harness();
        let size = h.stoat.size();

        let default_split = crate::render::picker::DEFAULT_LIST_PERCENT;
        let without =
            crate::render::commit_picker::commit_picker_layout(size, None, 0, default_split)
                .expect("fits");
        let with =
            crate::render::commit_picker::commit_picker_layout(size, Some(1), 0, default_split)
                .expect("fits");

        assert!(without.graph.is_none());
        let graph = with.graph.expect("one lane reserves a column");
        assert_eq!(graph.width, 3, "two cells for the lane plus a separator");
        assert_eq!(with.list.x, without.list.x + 3, "the table starts after it");
        assert_eq!(with.list.width, without.list.width - 3);
        assert_eq!(
            with.preview.map(|r| r.width),
            without.preview.map(|r| r.width),
            "the diff keeps the modal's full width"
        );
    }

    /// The table/diff separator is draggable, so the share the drag records has
    /// to move the boundary while the floor that keeps the history readable
    /// survives a drag all the way to the top.
    #[test]
    fn the_share_moves_the_table_diff_boundary_down_to_its_floor() {
        let size = seeded_picker_harness().stoat.size();
        let layout = |percent| {
            crate::render::commit_picker::commit_picker_layout(size, None, 0, percent)
                .expect("fits")
        };

        let default = layout(crate::render::picker::DEFAULT_LIST_PERCENT);
        let tall = layout(70);
        assert!(
            tall.list.height > default.list.height,
            "a larger share gives the table more rows: {} vs {}",
            tall.list.height,
            default.list.height
        );
        assert_eq!(
            tall.preview.map(|r| r.height),
            Some(
                default.list.height + default.preview.expect("a diff fits").height
                    - tall.list.height
            ),
            "and the diff gives up exactly what the table gained"
        );

        assert_eq!(
            layout(0).list.height,
            MIN_LIST_ROWS,
            "a share dragged to nothing still leaves the table its floor"
        );
    }

    #[test]
    fn a_rich_theme_strokes_the_graph_and_writes_no_glyphs() {
        let h = seeded_picker_harness();
        let p = h.stoat.commit_picker.as_ref().expect("open");
        let (buf, mut scene) = painted_graph(p, &h.stoat.theme, true);

        let frames = scene
            .buffer()
            .windows(b"polyline".len())
            .filter(|w| *w == b"polyline")
            .count();
        assert!(
            frames >= 3,
            "a three-commit history strokes at least one node per row, got {frames}"
        );
        assert!(
            buf.content().iter().all(|cell| cell.symbol() == " "),
            "the stroked path writes no glyphs"
        );
    }

    #[test]
    fn a_theme_without_rgb_falls_back_to_glyphs() {
        let h = seeded_picker_harness();
        let p = h.stoat.commit_picker.as_ref().expect("open");
        let (buf, mut scene) = painted_graph(p, &crate::theme::Theme::empty(), true);

        assert!(scene.buffer().is_empty(), "the fallback emits no frames");
        let painted: String = (0..3)
            .map(|y| buf.cell((0, y)).expect("in bounds").symbol().to_owned())
            .collect();
        assert_eq!(
            painted, "◉◉●",
            "each row's node lands in lane 0, and the two branch tips take the \
             ringed glyph while the commit under them takes the plain one"
        );

        let merge = picker(merge_history(), &[], "mmm0000");
        let (buf, _) = painted_graph(&merge, &crate::theme::Theme::empty(), true);
        assert_eq!(
            buf.cell((0, 0)).expect("in bounds").symbol(),
            "○",
            "and a merge takes the hollow glyph, the fallback's stand-in for \
             the ring the stroked path draws"
        );
    }

    /// A branch tip is the row a reader navigates by, so it has to be tellable
    /// from the history running past it at a glance.
    #[test]
    fn a_branch_tip_strokes_a_wider_node_in_a_stronger_color() {
        use crate::render::commit_picker::{BRANCH_NODE_DIAMETER, NODE_DIAMETER, NODE_HALO};

        let h = seeded_picker_harness();
        let p = h.stoat.commit_picker.as_ref().expect("open");
        let (_, mut scene) = painted_graph(p, &graph_theme(), true);
        let nodes = node_discs(&mut scene);

        let widths: Vec<u16> = nodes.iter().map(|&(width, _)| width).collect();
        assert_eq!(
            widths,
            [
                BRANCH_NODE_DIAMETER + NODE_HALO,
                BRANCH_NODE_DIAMETER,
                BRANCH_NODE_DIAMETER + NODE_HALO,
                BRANCH_NODE_DIAMETER,
                NODE_DIAMETER + NODE_HALO,
                NODE_DIAMETER,
            ],
            "main and feature sit on the first two rows and draw wide nodes, \
             leaving the commit below them an ordinary one, and each rides a \
             halo a touch wider than itself"
        );
        assert_eq!(
            nodes[0].1, GRAPH_BG,
            "the halo is drawn in the modal background, which is what cuts the \
             node out of the lane running through it"
        );
        assert_ne!(
            nodes[1].1, nodes[5].1,
            "and a tip keeps the raw lane color rather than the blended one \
             every other node shares"
        );
    }

    /// An RGB theme says nothing about whether the host can stroke a path. A
    /// foreign terminal running one used to get an empty column where the graph
    /// belongs, so the glyph lanes have to come back on the host, not the theme.
    #[test]
    fn a_dead_scene_draws_the_graph_as_glyphs_despite_an_rgb_theme() {
        let h = seeded_picker_harness();
        let p = h.stoat.commit_picker.as_ref().expect("open");
        let (buf, mut scene) = painted_graph(p, &graph_theme(), false);

        let painted: String = (0..3)
            .map(|y| buf.cell((0, y)).expect("in bounds").symbol().to_owned())
            .collect();
        assert_eq!(
            painted, "◉◉●",
            "the same glyph lanes a non-RGB theme has always drawn"
        );
        assert!(
            scene.buffer().is_empty(),
            "and no stroked path is built for a host that cannot show one"
        );
    }

    /// Where a row came from is history a lane line alone cannot show, so a
    /// merge's node has to carry it.
    #[test]
    fn a_merge_strokes_a_ring_rather_than_another_filled_dot() {
        use crate::render::commit_picker::{MERGE_HOLE, NODE_DIAMETER, NODE_HALO};

        let p = picker(merge_history(), &[], "mmm0000");
        let (_, mut scene) = painted_graph(&p, &graph_theme(), true);
        let nodes = node_discs(&mut scene);

        assert_eq!(
            nodes[0],
            (NODE_DIAMETER + NODE_HALO, GRAPH_BG),
            "the merge's halo goes down first"
        );
        assert_eq!(nodes[1].0, NODE_DIAMETER, "then the node over it");
        assert_ne!(nodes[1].1, GRAPH_BG, "in its own lane color");
        assert_eq!(
            nodes[2],
            (MERGE_HOLE, GRAPH_BG),
            "and the background hole last, which is what leaves a ring"
        );
        assert_eq!(
            nodes.len(),
            9,
            "only the two-parent row draws a third disc, the other three rows \
             a halo and a filled node each"
        );
    }

    /// A merge's diagonal is the absorbed branch's own line arriving at the
    /// merge, not something belonging to the row it lands on. Stroking it in
    /// the node's lane color would break that branch's line in two at the row
    /// boundary, where the run below it continues in its own color.
    #[test]
    fn a_merge_swoop_keeps_the_color_of_the_branch_it_absorbs() {
        let p = picker(merge_history(), &[], "mmm0000");
        let (_, mut scene) = painted_graph(&p, &graph_theme(), true);
        let colors: Vec<[u8; 3]> = lane_transitions(&mut scene)
            .iter()
            .map(|line| line.color)
            .collect();

        assert_eq!(
            colors.len(),
            2,
            "the feature lane opens under the merge and folds back two rows down"
        );
        assert_eq!(
            colors[0], colors[1],
            "both belong to the feature branch, so its line runs one color from \
             where it leaves the mainline up into the merge absorbing it"
        );
        assert_ne!(
            colors[0],
            node_discs(&mut scene)[1].1,
            "and that color is the feature lane's rather than the merge node's"
        );
    }

    /// A lane reads as a column only if it stays one right up to the bend, the
    /// way a git GUI draws it. A line that starts drifting sideways the moment
    /// it leaves a node reads as a diagonal, not as a branch leaving its lane.
    #[test]
    fn a_lane_transition_runs_straight_before_and_after_its_bend() {
        use crate::render::commit_picker::CURVE_STRAIGHT;

        let p = picker(merge_history(), &[], "mmm0000");
        let (_, mut scene) = painted_graph(&p, &graph_theme(), true);
        let transitions = lane_transitions(&mut scene);

        assert_eq!(
            transitions.len(),
            2,
            "the merge opens a second lane and the feature row folds it back"
        );

        for PolylineCommand { points, .. } in transitions {
            let (start, end) = (points[0], points[points.len() - 1]);
            let (bend_top, bend_bottom) = (start[1] + CURVE_STRAIGHT, end[1] - CURVE_STRAIGHT);

            assert_eq!(
                points[1],
                [start[0], bend_top],
                "the line leaves its node vertically"
            );
            assert_eq!(
                points[points.len() - 2],
                [end[0], bend_bottom],
                "and arrives at the next one vertically"
            );
            assert!(
                points[2..points.len() - 2]
                    .iter()
                    .all(|p| p[1] > bend_top && p[1] < bend_bottom),
                "with the whole bend confined to the middle of the row, got {points:?}"
            );
        }
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

    /// The picker covers a pane, so a press anywhere over it must not reach the
    /// editor beneath and move a cursor the user cannot see.
    #[test]
    fn a_press_over_the_list_never_reaches_the_buffer() {
        let mut h = seeded_picker_harness();
        h.seed_focused_buffer(&"line\n".repeat(200));
        h.snapshot();
        let head = h.primary_head_offset();
        let list = layout(&h).list;

        press(
            &mut h,
            MouseEventKind::Down(MouseButton::Left),
            list.x + 4,
            list.y + 1,
        );

        assert_eq!(
            h.primary_head_offset(),
            head,
            "the cursor in the covered editor stays where it was"
        );
    }

    /// Dragging the row between the table and the diff is how the user gives one
    /// of them more room, so the drag has to land the separator where the pointer
    /// is and both panes have to keep a usable floor.
    #[test]
    fn dragging_the_separator_moves_the_table_diff_split() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        let before = layout(&h);
        let separator = before.list.y + before.list.height;
        assert!(
            before.preview.is_some(),
            "the seeded modal is tall enough to show a diff"
        );

        press(
            &mut h,
            MouseEventKind::Down(MouseButton::Left),
            before.inner.x + 2,
            separator,
        );
        assert_eq!(
            h.stoat.modal_separator_drag,
            Some(ModalKind::CommitPicker),
            "a press on the separator arms the drag"
        );

        press(
            &mut h,
            MouseEventKind::Drag(MouseButton::Left),
            before.inner.x + 2,
            separator + 2,
        );
        let dragged = layout(&h);
        assert_eq!(
            dragged.list.height,
            before.list.height + 2,
            "the table grows to exactly where the pointer left the separator"
        );

        press(
            &mut h,
            MouseEventKind::Up(MouseButton::Left),
            before.inner.x + 2,
            separator + 2,
        );
        assert_eq!(
            h.stoat.modal_separator_drag, None,
            "releasing clears the arm"
        );

        press(
            &mut h,
            MouseEventKind::Drag(MouseButton::Left),
            before.inner.x + 2,
            separator + 5,
        );
        assert_eq!(
            layout(&h).list.height,
            dragged.list.height,
            "and a drag after the release moves nothing"
        );
    }

    #[test]
    fn a_separator_drag_clamps_at_both_floors() {
        let mut h = seeded_picker_harness();
        h.snapshot();
        let before = layout(&h);
        let separator = before.list.y + before.list.height;
        let column = before.inner.x + 2;
        let body_height = before.inner.y + before.inner.height - before.list.y;

        press(
            &mut h,
            MouseEventKind::Down(MouseButton::Left),
            column,
            separator,
        );
        press(
            &mut h,
            MouseEventKind::Drag(MouseButton::Left),
            column,
            before.inner.y + before.inner.height,
        );
        let bottom = layout(&h);
        assert_eq!(
            bottom.preview.map(|r| r.height),
            Some(MIN_PREVIEW_ROWS),
            "dragged to the modal's bottom the diff keeps its floor"
        );
        assert_eq!(
            bottom.list.height,
            body_height - MIN_PREVIEW_ROWS - 1,
            "and the table takes everything above it"
        );

        press(
            &mut h,
            MouseEventKind::Drag(MouseButton::Left),
            column,
            before.list.y,
        );
        assert_eq!(
            layout(&h).list.height,
            MIN_LIST_ROWS,
            "dragged to the top the table keeps its own floor"
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

    /// The picker is a boxed modal, so a press on one of its rows selects that
    /// row rather than reaching the pane it covers.
    #[test]
    fn a_press_on_a_commit_row_selects_it() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut h = seeded_picker_harness();
        h.snapshot();

        let lanes = h.stoat.commit_picker.as_ref().and_then(|p| p.graph_lanes);
        let layout = crate::render::commit_picker::commit_picker_layout(
            h.stoat.size(),
            lanes,
            crate::app::modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::CommitPicker),
            crate::app::modal_split_percent(&h.stoat.modal_split, ModalKind::CommitPicker),
        )
        .expect("the picker lays out");

        h.stoat.update(crate::test_fixture::mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            layout.list.x + 1,
            layout.list.y + 2,
        ));

        assert_eq!(
            h.stoat
                .commit_picker
                .as_ref()
                .expect("the press does not close the picker")
                .selected,
            2,
            "the press lands on the third row and selects it"
        );
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
    /// The walk arrives filtered by whatever was typed while it ran.
    ///
    /// Installing it unfiltered would not only show every row for an instant.
    /// The default selection is taken over the filtered list, so an unfiltered
    /// install lands the selection on a row the query excludes, and the preview
    /// build that follows is spent on it.
    #[test]
    fn a_walk_arrives_filtered_by_the_query_it_is_given() {
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
        let mut picker = CommitPicker::new(
            h.stoat.active_workspace_mut(),
            executor,
            CommitPickerRole::PickBase,
            PathBuf::from("/repo"),
            "c3d4e5f6".to_string(),
            SEEDED_NOW_EPOCH,
        );
        picker.set_commits(commits, branch_tips, "tweak");

        assert_eq!(picker.commits.len(), 3, "the whole walk is held");
        assert_eq!(
            picker.filtered.len(),
            1,
            "only the row the query names is listed",
        );
        assert_eq!(
            picker.selected_commit().map(|c| c.sha.clone()),
            Some("b2c3d4e5".to_string()),
            "and the selection starts on a row the query kept",
        );
    }

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
        let mut picker = CommitPicker::new(
            h.stoat.active_workspace_mut(),
            executor,
            CommitPickerRole::PickBase,
            PathBuf::from("/repo"),
            "c3d4e5f6".to_string(),
            SEEDED_NOW_EPOCH,
        );
        picker.set_commits(commits, branch_tips, "");
        h.stoat.commit_picker = Some(picker);
        h.settle();
        h
    }
}
