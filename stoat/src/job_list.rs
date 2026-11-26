//! The Job List, one Overlay over every Cmd that a user may want to find
//! again.
//!
//! It holds each Job of the Job Tray and each Cmd of the workspace's Run
//! panes.
//!
//! The list owns no Cmd. [`JobList::sync`] rebuilds its rows from the store on
//! each pass, so a Cmd that starts, ends, or leaves its place shows in the
//! rows at once, and the keys of the list act on the Cmd of the selected row
//! where it stands. The filter input and the ranking follow the session
//! finder's, through [`picker::rank_into`].

use crate::{
    fuzzy,
    input_view::InputView,
    picker,
    run::{CmdId, RunId},
    workspace::Workspace,
};
use std::{cmp::Reverse, time::Instant};

/// Where the Cmd of a row stands, which is where the row's keys act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobPlace {
    /// The Job Tray, whose Tray Chip carries the Cmd.
    Tray,
    /// The stack of the Run pane that shows the run.
    RunPane(RunId),
}

/// One row of the Job List.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobEntry {
    pub(crate) cmd: CmdId,
    pub(crate) place: JobPlace,
    /// The line of the Cmd, which the filter matches.
    pub(crate) line: String,
    /// Whether the Cmd still runs, which the `running` scope keeps.
    pub(crate) running: bool,
}

/// The rows that the Job List keeps, which `S-Tab` cycles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobScope {
    All,
    Running,
}

/// The open Job List, its filter input, and the ranking of its rows.
pub(crate) struct JobList {
    pub(crate) input: InputView,
    entries: Vec<JobEntry>,
    /// Indices into `entries` in display order after the filter and the
    /// scope.
    filtered: Vec<usize>,
    /// Matched character offsets in each filtered row's line, parallel to
    /// `filtered`, for the line's highlights.
    match_indices: Vec<Vec<u32>>,
    /// The parse behind the ranking, so a painted row past the indexed block
    /// derives its offsets rather than going unhighlighted.
    last_pattern: Option<fuzzy::Pattern>,
    /// Cursor into `filtered`.
    selected: usize,
    /// Rows the last render painted, stamped by the renderer and read by
    /// [`Self::page`].
    pub(crate) viewport_rows: Option<usize>,
    scope: JobScope,
    /// The query that the ranking came from, `None` until the first ranking.
    /// With unchanged rows, a pass of the same query ranks nothing, and
    /// [`Self::toggle_scope`] ranks at once.
    last_query: Option<String>,
}

impl JobList {
    /// An empty list over `input`, whose first [`Self::sync`] lists the rows.
    ///
    /// The caller builds the input, because creating it needs the active
    /// workspace mutably.
    pub(crate) fn new(input: InputView) -> Self {
        Self {
            input,
            entries: Vec::new(),
            filtered: Vec::new(),
            match_indices: Vec::new(),
            last_pattern: None,
            selected: 0,
            viewport_rows: None,
            scope: JobScope::All,
            last_query: None,
        }
    }

    /// Rebuild the rows from the store of `ws` and rank them for `query`.
    ///
    /// The rows are each Job of the tray, as [`JobPlace::Tray`], and each Cmd
    /// of each stack that is not in the tray and did not hand off to a terminal
    /// tab, as [`JobPlace::RunPane`], newest first by start time. A Cmd that
    /// handed off makes no row, because its run is a terminal tab. The
    /// selection stays on the same Cmd while the list still holds it. Rows and
    /// a query that the last ranking already saw rank nothing.
    pub(crate) fn sync(&mut self, ws: &Workspace, query: &str) {
        let entries = entries_of(ws);
        if self.last_query.as_deref() == Some(query) && entries == self.entries {
            return;
        }

        let kept = self.selected_entry().map(|entry| entry.cmd);
        self.entries = entries;
        self.rank(query, kept);
    }

    pub(crate) fn entries(&self) -> &[JobEntry] {
        &self.entries
    }

    pub(crate) fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn scope(&self) -> JobScope {
        self.scope
    }

    /// The row under the selection, or `None` for an empty list.
    pub(crate) fn selected_entry(&self) -> Option<&JobEntry> {
        self.entries.get(*self.filtered.get(self.selected)?)
    }

    /// Matched offsets to highlight in filtered `row`'s line.
    ///
    /// See [`picker::row_indices`] for what `scratch` and `matching` are for.
    pub(crate) fn row_indices<'a>(
        &'a self,
        row: usize,
        scratch: &'a mut Vec<u32>,
        matching: &mut fuzzy::Scratch,
    ) -> &'a [u32] {
        let line = self
            .filtered
            .get(row)
            .and_then(|&idx| self.entries.get(idx))
            .map(|entry| entry.line.as_str())
            .unwrap_or_default();
        picker::row_indices(
            &self.match_indices,
            self.last_pattern.as_ref(),
            row,
            line,
            scratch,
            matching,
        )
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        picker::nav_move(self.filtered.len(), &mut self.selected, delta);
    }

    /// Page the selection by half the rendered list height in `dir`, or by one
    /// row before the first render.
    pub(crate) fn page(&mut self, dir: i32) {
        let step = dir * picker::nav_page_step(self.viewport_rows);
        picker::nav_move(self.filtered.len(), &mut self.selected, step);
    }

    /// Flip the list between every row and the rows whose Cmd still runs, and
    /// rank again for the last query, keeping the selection on its Cmd.
    pub(crate) fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            JobScope::All => JobScope::Running,
            JobScope::Running => JobScope::All,
        };
        let query = self.last_query.take().unwrap_or_default();
        let kept = self.selected_entry().map(|entry| entry.cmd);
        self.rank(&query, kept);
    }

    /// Free the filter input's editor and scratch buffer, so the closed list
    /// leaves no unsaved buffer behind.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    /// Rank the rows in scope for `query`, and select the row of `kept` while
    /// the ranking keeps it.
    fn rank(&mut self, query: &str, kept: Option<CmdId>) {
        let scope = self.scope;
        let entries = &self.entries;
        let in_scope = |idx: &usize| scope == JobScope::All || entries[*idx].running;
        let items = entries
            .iter()
            .enumerate()
            .filter(|(idx, _)| in_scope(idx))
            .map(|(idx, entry)| (idx, entry.line.as_str()));

        self.last_pattern = picker::rank_into(
            query,
            items,
            entries.len(),
            &mut self.filtered,
            &mut self.match_indices,
        );
        // An unusable query lists every row without walking `items`, so the
        // scope narrows that list here. Its rows hold no offsets, so the
        // offset list only shrinks to stay parallel.
        if self.last_pattern.is_none() {
            self.filtered.retain(in_scope);
            self.match_indices.truncate(self.filtered.len());
        }
        self.last_query = Some(query.to_owned());

        if let Some(at) = kept.and_then(|cmd| {
            self.filtered
                .iter()
                .position(|&idx| self.entries[idx].cmd == cmd)
        }) {
            self.selected = at;
        }
        picker::nav_clamp(self.filtered.len(), &mut self.selected);
    }
}

/// The rows of `ws`, as [`JobList::sync`] lists them.
///
/// The sort is stable, so rows of one start time keep the tray's rows first.
fn entries_of(ws: &Workspace) -> Vec<JobEntry> {
    let tray = ws.jobs.iter().map(|&id| (id, JobPlace::Tray));
    let stacks = ws.runs.iter().flat_map(|(run, state)| {
        state
            .blocks
            .iter()
            .map(move |&id| (id, JobPlace::RunPane(run)))
    });

    let mut rows: Vec<(Option<Instant>, JobEntry)> = tray
        .chain(stacks)
        .filter_map(|(id, place)| {
            let cmd = ws.cmds.get(id)?;
            let listed = match place {
                JobPlace::Tray => true,
                JobPlace::RunPane(_) => !cmd.in_jobs && cmd.handoff.is_none(),
            };
            let entry = JobEntry {
                cmd: id,
                place,
                line: cmd.command.clone(),
                running: !cmd.finished,
            };
            listed.then_some((cmd.started, entry))
        })
        .collect();
    rows.sort_by_key(|(started, _)| Reverse(*started));
    rows.into_iter().map(|(_, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::JobPlace;
    use crate::{
        action_handlers,
        app::Stoat,
        keymap_state,
        render::job_list::{job_list_layout, job_list_rows},
        run::RunId,
        test_harness::{CmdHandle, TestHarness},
        theme::scope,
    };
    use std::time::Duration;
    use stoat_action::{Jobs, NewTab, SplitRight};

    /// The ended Cmd `echo a` and, a second newer, the Job `sleep 30`, on the
    /// stack of one Run pane, with the run of that pane.
    fn two_cmds(h: &mut TestHarness) -> (CmdHandle, CmdHandle, RunId) {
        let run = h.open_run();
        let echo = h.submit_run("echo a");
        h.inject_cmd_done(echo.cmd, 0);
        h.advance_clock(Duration::from_secs(1));
        let job = h.push_job("sleep 30");
        (echo, job, run)
    }

    /// The line of each row of the open list, in display order.
    fn lines(h: &TestHarness) -> Vec<String> {
        let list = h.stoat.job_list.as_ref().expect("the Job List is open");
        list.filtered()
            .iter()
            .map(|&idx| list.entries()[idx].line.clone())
            .collect()
    }

    #[test]
    fn space_j_j_lists_each_job_and_each_run_pane_cmd_newest_first() {
        let mut h = Stoat::test();
        let (_, _, run) = two_cmds(&mut h);
        h.type_keys("escape space J j");

        let list = h.stoat.job_list.as_ref().expect("the Job List is open");
        let entries: Vec<(&str, JobPlace)> = list
            .entries()
            .iter()
            .map(|entry| (entry.line.as_str(), entry.place))
            .collect();
        assert_eq!(
            entries,
            [
                ("sleep 30", JobPlace::Tray),
                ("echo a", JobPlace::RunPane(run))
            ]
        );
    }

    #[test]
    fn the_filter_narrows_the_list_by_the_line() {
        let mut h = Stoat::test();
        two_cmds(&mut h);
        h.type_keys("escape space J j");
        h.type_text("ech");
        h.settle();

        assert_eq!(lines(&h), ["echo a"]);
    }

    #[test]
    fn shift_tab_cycles_all_and_running() {
        let mut h = Stoat::test();
        two_cmds(&mut h);
        h.type_keys("escape space J j backtab");
        let running = lines(&h);
        h.type_keys("backtab");

        assert_eq!(
            [running, lines(&h)],
            [vec!["sleep 30"], vec!["sleep 30", "echo a"]]
        );
    }

    #[test]
    fn enter_on_a_tray_row_opens_the_job() {
        let mut h = Stoat::test();
        let (echo, job, run) = two_cmds(&mut h);
        h.type_keys("escape space J j enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.job_list.is_none(),
                ws.jobs.clone(),
                ws.runs[run].blocks.clone(),
                ws.cmds[job.cmd].in_jobs,
            ),
            (true, Vec::new(), vec![echo.cmd, job.cmd], false),
        );
    }

    /// `Enter` on a row of a Run pane gives that pane the focus from the file
    /// beside it, and the Job stays in the tray.
    #[test]
    fn enter_on_a_run_pane_row_focuses_its_pane() {
        let mut h = Stoat::test();
        let editor = h.stoat.active_workspace().panes.focus();
        action_handlers::dispatch(&mut h.stoat, &SplitRight);
        let (_, job, _) = two_cmds(&mut h);
        let run_pane = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.set_focus(editor);
        h.type_keys("space J j down enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.panes.focus() == run_pane, ws.jobs.clone()),
            (true, vec![job.cmd]),
        );
    }

    /// Over a Run pane, the arrows, `Ctrl-n` and `Ctrl-p`, and the page keys
    /// step the list's selection, and the prompt under it walks no history.
    #[test]
    fn up_and_down_move_the_selection_over_a_run_pane() {
        let mut h = Stoat::test();
        let (_, _, run) = two_cmds(&mut h);
        action_handlers::dispatch(&mut h.stoat, &Jobs);
        let steps: Vec<Option<usize>> = ["down", "up", "ctrl-n", "ctrl-p", "pagedown", "pageup"]
            .into_iter()
            .map(|key| {
                h.type_keys(key);
                h.stoat.job_list.as_ref().map(|list| list.selected())
            })
            .collect();

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (steps, ws.runs[run].input.text(ws)),
            ([1, 0, 1, 0, 1, 0].map(Some).to_vec(), String::new()),
        );
    }

    #[test]
    fn ctrl_r_ctrl_c_and_ctrl_x_act_on_the_selected_row() {
        let mut h = Stoat::test();
        let (echo, job, run) = two_cmds(&mut h);
        h.type_keys("escape space J j ctrl-c");
        let interrupted = (job.session.sent_bytes(), h.stoat.job_list.is_some());

        h.inject_cmd_done(job.cmd, 0);
        h.type_keys("ctrl-r");
        let rerun = h.stoat.active_workspace().cmds[job.cmd].run_count;

        h.inject_cmd_done(job.cmd, 0);
        h.type_keys("ctrl-x");
        let dismissed = h.stoat.active_workspace().jobs.clone();

        h.type_keys("down ctrl-x");
        let refused = h.stoat.pending_message.clone();
        h.type_keys("ctrl-r");

        let ws = h.stoat.active_workspace();
        let selected = h
            .stoat
            .job_list
            .as_ref()
            .and_then(|list| list.selected_entry())
            .map(|entry| entry.cmd);
        assert_eq!(
            (
                interrupted,
                rerun,
                dismissed,
                refused,
                (ws.cmds[echo.cmd].run_count, ws.runs[run].blocks.clone()),
                selected,
            ),
            (
                (vec![vec![0x03]], true),
                2,
                Vec::new(),
                Some("not in the tray".to_owned()),
                (2, vec![echo.cmd, job.cmd]),
                Some(echo.cmd),
            ),
            "the selection follows the Cmd that ran again to the top row",
        );
    }

    /// A Cmd of a Run pane that started after a Job lists above it, because
    /// the rows go by start time, not by place.
    #[test]
    fn a_newer_run_pane_cmd_lists_above_an_older_job() {
        let mut h = Stoat::test();
        h.open_run();
        h.push_job("sleep 30");
        h.advance_clock(Duration::from_secs(1));
        h.submit_run("echo a");
        action_handlers::dispatch(&mut h.stoat, &Jobs);

        assert_eq!(lines(&h), ["echo a", "sleep 30"]);
    }

    /// The list follows the store while it is open, so a Cmd that ends drops
    /// out of the `running` scope.
    #[test]
    fn a_cmd_that_ends_leaves_the_running_scope() {
        let mut h = Stoat::test();
        let (_, job, _) = two_cmds(&mut h);
        h.type_keys("escape space J j backtab");
        let running = lines(&h);
        h.inject_cmd_done(job.cmd, 0);
        h.settle();

        assert_eq!([running, lines(&h)], [vec!["sleep 30"], Vec::<&str>::new()]);
    }

    /// The scope switch keeps the selection on its Cmd, so a row picked among
    /// the running ones stays picked among all of them.
    #[test]
    fn shift_tab_keeps_the_selection_on_its_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let oldest = h.push_job("sleep 30");
        h.advance_clock(Duration::from_secs(1));
        let ended = h.submit_run("echo a");
        h.inject_cmd_done(ended.cmd, 0);
        h.advance_clock(Duration::from_secs(1));
        h.submit_run("sleep 31");
        h.type_keys("escape space J j backtab down backtab");

        let list = h.stoat.job_list.as_ref().expect("the Job List is open");
        assert_eq!(
            (lines(&h), list.selected_entry().map(|entry| entry.cmd)),
            (
                ["sleep 31", "echo a", "sleep 30"]
                    .map(str::to_owned)
                    .to_vec(),
                Some(oldest.cmd),
            ),
        );
    }

    /// `Jobs` with the list open keeps the list and the filter typed into it.
    #[test]
    fn jobs_with_the_list_open_keeps_it() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &Jobs);
        h.type_text("ech");
        action_handlers::dispatch(&mut h.stoat, &Jobs);

        let ws = h.stoat.active_workspace();
        let query = h.stoat.job_list.as_ref().map(|list| list.input.text(ws));
        assert_eq!(query.as_deref(), Some("ech"));
    }

    /// `Esc` closes the list, and the focused view is back in normal mode,
    /// not in the `Space J` submenu that opened the list.
    #[test]
    fn escape_closes_the_list_into_normal_mode() {
        let mut h = Stoat::test();
        h.type_keys("space J j escape");

        assert_eq!(
            (h.stoat.job_list.is_none(), h.stoat.focused_mode()),
            (true, "normal")
        );
    }

    /// A close from outside the list, as a workspace switch closes the
    /// topmost modal, disposes the list's input.
    #[test]
    fn a_close_from_outside_disposes_the_list() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &Jobs);
        let editor = h.stoat.job_list.as_ref().map(|list| list.input.editor_id);
        keymap_state::close_topmost_modal(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.job_list.is_none(),
                editor.is_some_and(|id| ws.editors.contains_key(id))
            ),
            (true, false),
        );
    }

    #[test]
    fn ctrl_c_on_an_ended_row_closes_the_list() {
        let mut h = Stoat::test();
        two_cmds(&mut h);
        h.type_keys("escape space J j down ctrl-c");

        assert_eq!(
            (
                h.stoat.job_list.is_none(),
                keymap_state::active_modal(&h.stoat)
            ),
            (true, None),
        );
    }

    #[test]
    fn a_handed_off_cmd_makes_no_row() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("echo a");
        h.submit_run("htop");
        action_handlers::dispatch(&mut h.stoat, &Jobs);

        assert_eq!(lines(&h), ["echo a"]);
    }

    #[test]
    fn an_empty_job_list_reads_no_jobs() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        action_handlers::dispatch(&mut h.stoat, &Jobs);
        h.snapshot();

        let (_, inner) = job_list_layout(h.stoat.size(), 0).expect("the list fits");
        let row = job_list_rows(inner);
        let buf = h.rendered_buffer();
        let cells = (row.x + 3..row.x + 10).map(|x| &buf[(x, row.y)]);
        let text: String = cells.clone().map(|cell| cell.symbol()).collect();
        let disabled = h.stoat.theme.get(scope::UI_TEXT_DISABLED).fg;
        assert_eq!(
            (text, cells.map(|cell| Some(cell.fg)).collect::<Vec<_>>()),
            ("no jobs".to_owned(), vec![disabled; 7]),
        );
    }

    #[test]
    fn the_jobs_command_opens_the_list() {
        let mut h = Stoat::test();
        h.type_text(":jobs");
        h.type_keys("enter");

        assert_eq!(keymap_state::modal_predicate(&h.stoat), Some("job_list"));
    }

    /// A running Cmd in a Run pane of a parked tab arms no tick, until the list
    /// opens and its row shows the Cmd's time.
    #[test]
    fn the_open_list_ticks_a_running_cmd_of_a_parked_tab() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("sleep 30");
        action_handlers::dispatch(&mut h.stoat, &NewTab);
        h.snapshot();
        let parked = h.stoat.cmd_tick.is_some();
        action_handlers::dispatch(&mut h.stoat, &Jobs);
        h.snapshot();
        let listed = h.stoat.cmd_tick.is_some();
        h.advance_clock(Duration::from_secs(1));
        h.snapshot();

        let text = h.rendered_text();
        let row = text.lines().find(|row| row.contains("sleep 30"));
        assert_eq!(
            (
                parked,
                listed,
                row.is_some_and(|row| row.contains("running 0:01"))
            ),
            (false, true, true),
            "{row:?}",
        );
    }

    #[test]
    fn snapshot_job_list_listing() {
        let mut h = TestHarness::with_size(80, 24);
        two_cmds(&mut h);
        h.type_keys("escape space J j");
        h.assert_snapshot("job_list_listing");
    }
}
