use super::pane::EditorDisposal;
use crate::{
    app::{Stoat, UpdateEffect},
    commit_list::{CommitListId, CommitListState},
    display_map::syntax_theme::SyntaxStyles,
    pane::{PaneId, View},
    review_session::DiffDocument,
    workspace::{diff::BaseHighlightCache, Workspace},
};
use std::sync::Arc;

const COMMITS_INITIAL_PAGE: usize = 64;
const COMMITS_PREFETCH_GAP: usize = 8;
const COMMITS_PAGE_STEP: usize = 16;

#[derive(Copy, Clone, Debug)]
pub(crate) enum CommitStep {
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    First,
    Last,
    /// The row at a 0-based index, clamped to the last loaded commit.
    Nth(usize),
}

/// Show a commits list in the focused pane, over the view the pane showed.
///
/// The bare command toggles the list, as `:diff` does, and `CommitsRefresh`
/// stays the reload. A diff opened from the list stands over it, so there the
/// command leaves the diff, and the covered list returns with its selection and
/// loaded pages intact.
///
/// Each pane opens a list of its own, so a list in another pane or another tab
/// keeps its place. Outside a git repository nothing changes.
pub(super) fn open_commits(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().focused_commits_id().is_some() {
        return close_commits(stoat);
    }
    let focus = stoat.active_workspace().panes.focus();
    if covered_commits(stoat.active_workspace(), focus).is_some() {
        super::review::exit_diff_view(stoat);
        restore_covered_commits(stoat, focus);
        return UpdateEffect::Redraw;
    }

    let covered = stoat.active_workspace().panes.pane(focus).view.clone();
    let Some(id) = spawn_commit_list(stoat, Some(covered)) else {
        return UpdateEffect::None;
    };

    // The diff ends before the list covers its editor, as the user ends one by
    // hand, so the editor comes back plain when the list closes. The helper
    // gates itself, so a plain pane, widened or not, is untouched. It reads the
    // focused editor, which is why it runs before the list takes the pane.
    super::review::exit_diff_view(stoat);

    let panes = &mut stoat.active_workspace_mut().panes;
    panes.pane_mut(focus).view = View::Commits(id);
    panes.widen(focus);

    drain_commits_tasks(stoat, id);
    ensure_selected_preview(stoat, id);
    drain_commits_tasks(stoat, id);
    UpdateEffect::Redraw
}

/// Start a fresh list for every commits pane of the active workspace whose list
/// did not survive, then point the pane at it.
///
/// The panes of every tab count, a parked one included, so a tab shows a live
/// list when the reader next switches to it.
///
/// A commits pane rides `PaneTree` serde as [`View::Commits`], but the list is
/// live state the session file does not hold, so the id is dead after a restore
/// or a workspace copy. Each dead pane gets a list of its own with its first
/// page loading. The selection, the scroll, and the view the list covered start
/// over, and a pane outside a git repository gets a `Commits (closed)` label.
pub(crate) fn respawn_commits_panes(stoat: &mut Stoat) {
    let dead_panes = {
        let ws = stoat.active_workspace();
        let dead =
            |view: &View| matches!(view, View::Commits(id) if !ws.commit_lists.contains_key(*id));
        ws.pane_trees()
            .enumerate()
            .flat_map(|(tree, panes)| {
                panes
                    .split_pane_ids()
                    .into_iter()
                    .filter(move |&id| dead(&panes.pane(id).view))
                    .map(move |id| (tree, id))
            })
            .collect::<Vec<(usize, PaneId)>>()
    };

    // `pane_trees_mut` walks the trees in the order `pane_trees` does, so the
    // index collected above names the same tree here.
    for (tree, pane_id) in dead_panes {
        let view = match spawn_commit_list(stoat, None) {
            Some(id) => View::Commits(id),
            None => View::Label("Commits (closed)".into()),
        };
        if let Some(panes) = stoat.active_workspace_mut().pane_trees_mut().nth(tree) {
            panes.pane_mut(pane_id).view = view;
        }
    }
}

/// Start a commits list over the active workspace's repository, with its first
/// page loading, and add it to the workspace's lists.
///
/// `covered` is the view the list replaces in its pane, which closing the list
/// puts back. Returns `None` and adds nothing outside a git repository.
fn spawn_commit_list(stoat: &mut Stoat, covered: Option<View>) -> Option<CommitListId> {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        tracing::warn!("spawn_commit_list: not inside a git repository");
        return None;
    };
    let Some(workdir) = repo.workdir() else {
        tracing::warn!("spawn_commit_list: git repo has no workdir");
        return None;
    };

    let mut state = CommitListState::new(workdir, repo.clone());
    state.covered = covered;
    state.pending_load = Some(spawn_commit_log_load(
        &stoat.executor,
        repo,
        None,
        COMMITS_INITIAL_PAGE,
        stoat.redraw_notify.clone(),
    ));
    Some(stoat.active_workspace_mut().commit_lists.insert(state))
}

/// Close the list the focused pane shows and put back the view it covered.
///
/// A covered view that is gone closes the pane instead. The pane tree keeps
/// its last pane, so a last pane gets a fresh scratch editor.
pub(super) fn close_commits(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_commits_id() else {
        return UpdateEffect::None;
    };
    let covered = ws.commit_lists.remove(id).and_then(|list| list.covered);
    let focus = ws.panes.focus();
    if ws.panes.widened() == Some(focus) {
        ws.panes.unwiden();
    }

    if super::pane::view_is_live(ws, covered.as_ref()) {
        super::pane::show_view_or_scratch(stoat, focus, covered);
    } else if !super::pane::close_pane_by_id(stoat, focus) {
        super::pane::show_view_or_scratch(stoat, focus, None);
    }
    stoat.set_focused_mode("normal".to_string());
    UpdateEffect::Redraw
}

/// The live list pane `pane` covers, where a diff opened from the list stands
/// in front of it.
pub(crate) fn covered_commits(ws: &Workspace, pane: PaneId) -> Option<CommitListId> {
    match ws.panes.pane(pane).prev_view {
        Some(View::Commits(id)) if ws.commit_lists.contains_key(id) => Some(id),
        _ => None,
    }
}

/// Put the list pane `pane` covers back on screen, in place of the editor a
/// diff opened from the list put in front of it.
///
/// Returns `false` and changes nothing when the pane covers no live list. The
/// editor in front goes unless another pane shows it, and the pane widens
/// again, since the widen belongs to the list.
pub(crate) fn restore_covered_commits(stoat: &mut Stoat, pane: PaneId) -> bool {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let Some(id) = covered_commits(ws, pane) else {
        return false;
    };

    let shown = ws.panes.pane_mut(pane);
    shown.prev_view = None;
    let front = std::mem::replace(&mut shown.view, View::Commits(id));
    super::pane::dispose_view(ws, &executor, front, EditorDisposal::GcIfUnreferenced);
    ws.panes.widen(pane);
    true
}

pub(crate) fn commits_step(stoat: &mut Stoat, step: CommitStep) -> UpdateEffect {
    let Some(id) = stoat.active_workspace().focused_commits_id() else {
        return UpdateEffect::None;
    };
    let moved = {
        let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) else {
            return UpdateEffect::None;
        };
        let moved = match step {
            CommitStep::Up(n) => state.move_up(n),
            CommitStep::Down(n) => state.move_down(n),
            CommitStep::PageUp => state.move_up(COMMITS_PAGE_STEP),
            CommitStep::PageDown => state.move_down(COMMITS_PAGE_STEP),
            CommitStep::First => state.move_to_first(),
            CommitStep::Last => state.move_to_last(),
            CommitStep::Nth(index) => state.move_to(index),
        };
        let height = state.viewport_rows;
        state.ensure_selected_visible(height);
        // The offset belongs to the diff the selection rests on, so a new
        // commit starts its preview at the top rather than partway down
        // whatever the last one was scrolled to.
        if moved {
            state.preview_scroll = 0;
        }
        moved
    };
    if !moved {
        return UpdateEffect::None;
    }
    maybe_spawn_next_page(stoat, id);
    ensure_selected_preview(stoat, id);
    drain_commits_tasks(stoat, id);
    UpdateEffect::Redraw
}

/// Scroll the detail pane's diff by `rows`, negative toward the top.
///
/// The render clamps the offset against the diff's length and the pane's
/// height, so this only has to keep it off the top. Nothing here knows how
/// long the diff is, and a second answer to that question drifts from the
/// render's.
pub(crate) fn commits_detail_scroll(stoat: &mut Stoat, rows: i32) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().focused_commits_mut() else {
        return UpdateEffect::None;
    };
    let by = rows.unsigned_abs() as usize;
    state.preview_scroll = match rows > 0 {
        true => state.preview_scroll.saturating_add(by),
        false => state.preview_scroll.saturating_sub(by),
    };
    UpdateEffect::Redraw
}

/// Step the detail pane by half its height, the way every list modal's
/// preview steps under the same keys.
///
/// The pane's height comes from the same rect the pointer hit-tests, so a key
/// step and a wheel notch read one geometry.
pub(super) fn commits_detail_half_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let ws = stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus()).area;
    let Some(rect) = crate::render::commits::commits_detail_rect(pane, stoat.commits_split) else {
        return UpdateEffect::None;
    };
    commits_detail_scroll(stoat, (rect.height as i32 / 2).max(1) * dir)
}

pub(super) fn commits_refresh(stoat: &mut Stoat) -> UpdateEffect {
    let Some(id) = stoat.active_workspace().focused_commits_id() else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat
        .active_workspace()
        .commit_lists
        .get(id)
        .map(|s| s.repo.clone())
    else {
        return UpdateEffect::None;
    };
    let task = spawn_commit_log_load(
        &stoat.executor,
        repo,
        None,
        COMMITS_INITIAL_PAGE,
        stoat.redraw_notify.clone(),
    );
    let ws = stoat.active_workspace_mut();
    if let Some(state) = ws.commit_lists.get_mut(id) {
        state.commits.clear();
        state.reached_end = false;
        state.selected = 0;
        state.scroll_top = 0;
        state.preview_scroll = 0;
        state.summaries.clear();
        state.preview_sessions.clear();
        state.pending_preview = None;
        state.requested_preview = None;
        state.pending_load = Some(task);
    }
    drain_commits_tasks(stoat, id);
    ensure_selected_preview(stoat, id);
    drain_commits_tasks(stoat, id);
    UpdateEffect::Redraw
}

/// Start the next page load for list `id` when its selection comes near the
/// tail of the loaded window.
///
/// Does nothing while a load is in flight or after the walk reached a root
/// commit.
fn maybe_spawn_next_page(stoat: &mut Stoat, id: CommitListId) {
    let Some(state) = stoat.active_workspace().commit_lists.get(id) else {
        return;
    };
    if state.pending_load.is_some() || state.reached_end {
        return;
    }
    let loaded = state.commits.len();
    if loaded == 0 {
        return;
    }
    let within_prefetch = state.selected + COMMITS_PREFETCH_GAP >= loaded;
    if !within_prefetch {
        return;
    }
    let last_sha = state.commits[loaded - 1].sha.clone();
    let repo = state.repo.clone();
    let task = spawn_commit_log_load(
        &stoat.executor,
        repo,
        Some(last_sha),
        COMMITS_INITIAL_PAGE,
        stoat.redraw_notify.clone(),
    );
    if let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) {
        state.pending_load = Some(task);
    }
}

/// Spawn a background preview build for the selection of list `id` if one is
/// not already cached, and no build is in flight for any commit of that list.
/// The summary lands with it, out of the same task.
///
/// Only one build runs at a time in a list. Dropping a [`stoat_scheduler::Task`]
/// leaves the blocking pool running the closure regardless, so a build per row
/// stacks one for every row scrolled past ahead of the row that matters.
/// [`pump_commits`] returns here once the running build lands, which is what
/// carries the selection's latest position through.
fn ensure_selected_preview(stoat: &mut Stoat, id: CommitListId) {
    let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) else {
        return;
    };
    let Some(sha) = state.selected_sha().map(str::to_string) else {
        return;
    };
    let workdir = state.workdir.clone();
    let repo = state.repo.clone();
    if state.preview_sessions.mark_used(&sha) || state.pending_preview.is_some() {
        return;
    }

    let task = spawn_commit_preview_load(
        &stoat.executor,
        repo,
        workdir,
        sha.clone(),
        stoat.language_registry.clone(),
        stoat.redraw_notify.clone(),
        PreviewHighlights::from_stoat(stoat),
    );

    if let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) {
        state.requested_preview = Some(sha.clone());
        state.pending_preview = Some(crate::commit_list::PendingPreview { sha, task });
    }
}

/// Poll both pending tasks of list `id` once, landing whichever finished.
///
/// Every action handler that touches a list calls this, so tests which settle
/// the scheduler see consistent state on the next render.
fn drain_commits_tasks(stoat: &mut Stoat, id: CommitListId) {
    let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) else {
        return;
    };
    state.poll_pending_load();
    state.poll_pending_preview();
}

/// Pull the finished tasks of every open list into its state, and spawn the
/// work those landings unlock, such as the preview a first page makes
/// possible. Returns true when any task landed or a new task was spawned.
///
/// Every list in the active workspace loads, not only the focused one, so a
/// list in a parked tab or an unfocused pane has its pages and its preview
/// when the reader comes back.
///
/// Called at the top of every `Stoat::render` tick so the UI reflects
/// settled state without requiring navigation input. Also called in the
/// test harness's `settle` loop so `assert_snapshot` sees terminal state
/// regardless of how many scheduler ticks the work needs.
pub(crate) fn pump_commits(stoat: &mut Stoat) -> bool {
    let ids: Vec<CommitListId> = stoat.active_workspace().commit_lists.keys().collect();
    let mut changed = false;
    for id in ids {
        changed |= pump_commit_list(stoat, id);
    }
    changed
}

/// Land the finished tasks of list `id` and spawn what they unlock, reporting
/// whether anything landed or started.
fn pump_commit_list(stoat: &mut Stoat, id: CommitListId) -> bool {
    let landed = {
        let Some(state) = stoat.active_workspace_mut().commit_lists.get_mut(id) else {
            return false;
        };
        let a = state.poll_pending_load();
        let b = state.poll_pending_preview();
        a || b
    };
    let spawned_before = {
        let Some(state) = stoat.active_workspace().commit_lists.get(id) else {
            return landed;
        };
        state.pending_load.is_some() || state.pending_preview.is_some()
    };
    ensure_selected_preview(stoat, id);
    maybe_spawn_next_page(stoat, id);
    let spawned_after = {
        let Some(state) = stoat.active_workspace().commit_lists.get(id) else {
            return landed;
        };
        state.pending_load.is_some() || state.pending_preview.is_some()
    };
    landed || (spawned_after && !spawned_before)
}

/// Spawn the blocking log walk, waking the run loop through `redraw` when the
/// page lands.
///
/// The pump reading this task polls with a noop waker, so the wake is what makes
/// a page appear on its own rather than waiting for the next input event to
/// happen to drive the pumps.
fn spawn_commit_log_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    after: Option<String>,
    limit: usize,
    redraw: Arc<tokio::sync::Notify>,
) -> stoat_scheduler::Task<Vec<crate::host::CommitInfo>> {
    executor.spawn_blocking(move || {
        let loaded = repo.log_commits(after.as_deref(), limit);
        redraw.notify_one();
        loaded
    })
}

/// What a preview build needs to bake syntax colors into its session.
///
/// The three pieces travel together because they are only ever used together,
/// and they cross into a blocking closure, so they are owned rather than
/// borrowed. An `attach` with highlighting off is a no-op, which is what makes
/// a preview match a syntax-off editor.
#[derive(Clone)]
pub(super) struct PreviewHighlights {
    pub(super) styles: SyntaxStyles,
    pub(super) cache: BaseHighlightCache,
    pub(super) enabled: bool,
}

impl PreviewHighlights {
    /// Read what a preview build needs off `stoat`.
    pub(super) fn from_stoat(stoat: &Stoat) -> Self {
        Self {
            styles: stoat.syntax_styles.clone(),
            cache: stoat.base_highlights_cache.clone(),
            enabled: stoat.syntax_highlight,
        }
    }

    /// Bake both sides' spans into `session`, or nothing when highlighting is
    /// off.
    pub(super) fn attach(&self, doc: &mut DiffDocument) {
        if !self.enabled {
            return;
        }
        super::review::attach_preview_highlights(doc, &self.styles, &self.cache);
    }
}
/// Spawn the blocking summary read and diff build for `sha`, waking the run
/// loop through `redraw` when they land.
///
/// The wake is what makes the preview appear on its own, for the reason
/// [`spawn_commit_log_load`] describes. It fires on a failed tree read too,
/// since the pump has a pending handle to clear either way.
fn spawn_commit_preview_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    workdir: std::path::PathBuf,
    sha: String,
    language_registry: Arc<stoat_language::LanguageRegistry>,
    redraw: Arc<tokio::sync::Notify>,
    highlights: PreviewHighlights,
) -> stoat_scheduler::Task<crate::commit_list::PreviewLoad> {
    executor.spawn_blocking(move || {
        // Read here rather than on the run loop. The tree walk behind it costs
        // tens of milliseconds on a wide commit, which held a frame per row
        // while the selection moved. One walk answers both halves.
        let (summary, changes) = match repo.commit_changes(&sha) {
            Some(both) => both,
            // A commit the walk cannot read leaves the whole tree as the
            // change, which is what a shallow clone's first commit is.
            None => (
                repo.commit_file_changes(&sha),
                repo.changed_contents(None, &sha).unwrap_or_default(),
            ),
        };
        let document =
            super::review::build_document_from_changes(&language_registry, &workdir, changes).map(
                |mut doc| {
                    highlights.attach(&mut doc);
                    doc
                },
            );
        redraw.notify_one();
        crate::commit_list::PreviewLoad {
            summary: Some(summary),
            document,
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        app::Stoat,
        commit_list::{CommitListId, Preview},
        pane::View,
        run::pty::PtyNotification,
    };

    /// The commits view shares the picker's preview cache, and shared the same
    /// defect: a commit that changed nothing produced no document, the answer
    /// was dropped, and the pump asked for the same build on every pass.
    #[test]
    fn a_commit_with_no_diff_stops_the_commits_view_asking() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "bbbb2222",
                    "chore: touch nothing",
                    &[("a.rs", "fn a() {}\n")],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "bbbb2222")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();

        h.type_text(":commits");
        h.type_keys("enter");
        h.settle();

        let state = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state");
        assert_eq!(
            state.selected_sha(),
            Some("bbbb2222"),
            "the empty commit is the one selected",
        );
        assert!(
            state.pending_preview.is_none(),
            "no build is left in flight for a commit that has no diff",
        );
        assert!(
            matches!(state.preview_sessions.get("bbbb2222"), Preview::Empty),
            "the empty answer is cached, not left looking unbuilt",
        );
    }

    /// Open `:commits` over a two-commit history and settle, leaving the newer
    /// commit selected.
    fn open_two_commit_history(h: &mut crate::test_harness::TestHarness) {
        h.seed_linear_history(
            "/repo",
            &[
                ("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "bbbb2222",
                    "feat: add b.rs",
                    // Carries a.rs forward, so the newer commit is one addition
                    // rather than an addition beside a deletion.
                    &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "bbbb2222")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();

        h.type_text(":commits");
        h.type_keys("enter");
        h.settle();
    }

    /// Reading the summary took a tree walk, and a wide commit made that tens of
    /// milliseconds. Doing it where the selection moves held a frame per row,
    /// so the read has to be in the spawned task and nowhere else.
    ///
    /// The absence is what this pins: right after the selection lands on a
    /// commit, nothing about that commit has been read yet.
    #[test]
    fn stepping_the_selection_reads_no_summary_on_the_run_loop() {
        let mut h = Stoat::test();
        open_two_commit_history(&mut h);

        let sha = {
            let state = h
                .stoat
                .active_workspace_mut()
                .focused_commits_mut()
                .expect("commits state");
            state.selected = 1;
            state.selected_sha().expect("the older commit").to_string()
        };

        let list = h
            .stoat
            .active_workspace()
            .focused_commits_id()
            .expect("the list pane");
        super::ensure_selected_preview(&mut h.stoat, list);

        let state = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state");
        assert_eq!(
            (
                state.summaries.contains_key(&sha),
                state.pending_preview.is_some(),
            ),
            (false, true),
            "the step spawned the read rather than doing it",
        );
    }

    /// The summary and the preview come out of one task, so the row that paints
    /// "loading summary..." is the same row that has no diff yet, and both
    /// arrive together rather than one on the loop and one off it.
    #[test]
    fn a_commits_summary_lands_with_its_preview() {
        let mut h = Stoat::test();
        open_two_commit_history(&mut h);

        let state = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state");
        let sha = state.selected_sha().expect("a selected commit");

        assert_eq!(
            (
                state.summaries.get(sha).map(Vec::len),
                matches!(state.preview_sessions.get(sha), Preview::Built(_)),
            ),
            (Some(1), true),
            "the commit's one changed file and its diff both landed",
        );
    }

    /// A list opened over a diff ends the diff first, as the user ends one by
    /// hand. The editor under the list keeps no flag and the pane keeps no
    /// latch, so the editor comes back plain when the list closes.
    #[test]
    fn opening_the_commits_list_exits_an_open_diff() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.seed_focused_buffer("changed\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        h.settle();
        assert_eq!(
            h.stoat.current_view(),
            Some("diff"),
            "the diff screen is what the frame paints"
        );

        h.open_commits("/repo");

        let ws = h.stoat.active_workspace();
        let latched = ws.panes.pane(ws.panes.focus()).diff_mode;
        let widened = ws.panes.widened();
        let diff_view = match ws.focused_commits().and_then(|list| list.covered.as_ref()) {
            Some(View::Editor(id)) => ws.editors.get(*id).map(|editor| editor.diff_view),
            _ => None,
        };
        assert_eq!(
            (
                h.stoat.current_view(),
                diff_view,
                latched,
                widened.is_some()
            ),
            (Some("commits"), Some(false), false, true),
            "the list paints widened over its editor, and the diff left no flag or latch behind"
        );
    }

    #[test]
    fn closing_the_commits_list_restores_the_layout() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.open_commits("/repo");
        let focus = h.stoat.active_workspace().panes.focus();
        let while_open = h.stoat.active_workspace().panes.widened();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseCommits);

        assert_eq!(
            (while_open, h.stoat.active_workspace().panes.widened()),
            (Some(focus), None),
            "the list widens the focused pane, and closing it restores the split"
        );
    }

    #[test]
    fn leaving_a_diff_over_the_list_keeps_the_list_widened() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.open_commits("/repo");

        let mut views = Vec::new();
        let review: [&dyn stoat_action::Action; 2] =
            [&stoat_action::CommitsOpenReview, &stoat_action::ReviewDone];
        for action in review {
            crate::action_handlers::dispatch(&mut h.stoat, action);
            h.settle();
            views.push(h.stoat.current_view());
        }

        assert_eq!(
            (views, h.stoat.active_workspace().panes.widened().is_some()),
            (vec![Some("diff"), Some("commits")], true),
            "the diff opens over the list, and its exit leaves the widen to the list"
        );
    }

    #[test]
    fn a_second_commits_command_closes_the_list() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.open_commits("/repo");
        let opened = h.stoat.current_view();

        h.open_commits("/repo");

        assert_eq!(
            (
                opened,
                h.stoat.current_view(),
                h.stoat.active_workspace().focused_commits().is_none()
            ),
            (Some("commits"), Some("file"), true),
            "the first command opens the list and the second closes it"
        );
    }

    /// A diff opened from the list stands over it, so the command leaves the
    /// diff rather than closing the list beneath, and the list keeps its
    /// selection because it is the same list rather than a reload.
    #[test]
    fn the_commits_command_over_a_diff_returns_to_the_list() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");
        h.type_keys("j");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitsOpenReview);
        h.settle();
        let over_diff = h.stoat.current_view();

        h.open_commits("/repo");

        assert_eq!(
            (
                over_diff,
                h.stoat.current_view(),
                selected(&h),
                h.stoat.active_workspace().commit_lists.len()
            ),
            (Some("diff"), Some("commits"), "a1b2c3d4".to_string(), 1),
            "the command over the diff returns to the list with its selection intact, \
             and opens no second list"
        );
    }

    #[test]
    fn a_new_tab_shows_its_own_editor_beside_an_open_list() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        let new_tab = (
            h.stoat.current_view(),
            h.stoat.active_workspace().focused_commits().is_none(),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleTab);

        assert_eq!(
            (new_tab, h.stoat.current_view()),
            ((Some("file"), true), Some("commits")),
            "the new tab shows its own editor, and the list waits in the tab that opened it"
        );
    }

    #[test]
    fn each_tab_opens_a_list_of_its_own() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_commits("/repo");
        h.type_keys("j");
        let here = selected(&h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleTab);

        assert_eq!(
            (
                h.stoat.active_workspace().commit_lists.len(),
                here,
                selected(&h)
            ),
            (2, "a1b2c3d4".to_string(), "b2c3d4e5".to_string()),
            "each tab holds a list of its own, with a selection of its own"
        );
    }

    /// The first list opens with nothing settled, so its first page is still
    /// out when a new tab parks it. One pump pass reaches every list, so the
    /// pages land together rather than one list behind another.
    #[test]
    fn every_open_list_keeps_loading() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommits);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommits);

        h.run_until_parked();
        super::pump_commits(&mut h.stoat);
        let first_pass: Vec<bool> = h
            .stoat
            .active_workspace()
            .commit_lists
            .values()
            .map(|list| !list.commits.is_empty())
            .collect();
        h.settle();

        let loaded: Vec<(bool, bool)> = h
            .stoat
            .active_workspace_mut()
            .commit_lists
            .values_mut()
            .map(|list| {
                let sha = list.selected_sha().map(str::to_string);
                let previewed = sha.is_some_and(|sha| list.preview_sessions.mark_used(&sha));
                (!list.commits.is_empty(), previewed)
            })
            .collect();
        assert_eq!(
            (first_pass, loaded),
            (vec![true, true], vec![(true, true), (true, true)]),
            "one pass lands the first page of every list, and the parked tab's list has its \
             preview as the focused one does"
        );
    }

    #[test]
    fn closing_the_list_returns_the_pane_to_the_buffer_it_covered() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.seed_focused_buffer("covered\n");
        let covered = h.stoat.focused_editor_ids().expect("a focused editor");
        h.open_commits("/repo");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseCommits);

        assert_eq!(
            h.stoat.focused_editor_ids(),
            Some(covered),
            "the pane shows the editor the list covered"
        );
    }

    #[test]
    fn a_walk_opened_from_the_list_returns_to_it() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        walk_from_the_list(&mut h);
        let walking = h.stoat.current_view();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                walking,
                h.stoat.current_view(),
                ws.panes.widened().is_some(),
                ws.editors.len(),
                ws.panes.pane(ws.panes.focus()).prev_view.is_none()
            ),
            (Some("diff"), Some("commits"), true, 1, true),
            "the walk's diff stands over the list, and the walk's end returns to the \
             list with the walk's editor gone and no record of the list left behind"
        );
    }

    #[test]
    fn closing_a_tab_drops_its_list() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_commits("/repo");
        let open = h.stoat.active_workspace().commit_lists.len();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseTab);

        assert_eq!(
            (open, h.stoat.active_workspace().commit_lists.len()),
            (1, 0),
            "the list closes with its tab"
        );
    }

    #[test]
    fn closing_a_list_pane_drops_the_editor_the_list_covered() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let (covered, _) = h.stoat.focused_editor_ids().expect("a focused editor");
        h.open_commits("/repo");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.commit_lists.len(), ws.editors.contains_key(covered)),
            (0, false),
            "the list and the editor it covered close with the pane"
        );
    }

    #[test]
    fn turning_off_a_diff_over_the_list_keeps_the_widen() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        walk_from_the_list(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });

        assert_eq!(
            (
                h.stoat.current_view(),
                h.stoat.active_workspace().panes.widened().is_some()
            ),
            (Some("file"), true),
            "the diff closes, and the widen stays with the list beneath"
        );
    }

    #[test]
    fn the_list_comes_back_widened_after_the_walk_drops_the_widen() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        walk_from_the_list(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::TogglePaneWiden);
        let dropped = h.stoat.active_workspace().panes.widened().is_none();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        assert_eq!(
            (
                dropped,
                h.stoat.current_view(),
                h.stoat.active_workspace().panes.widened().is_some()
            ),
            (true, Some("commits"), true),
            "the list takes the full width again when it comes back"
        );
    }

    #[test]
    fn closing_a_list_over_a_label_closes_the_pane_or_falls_back_to_a_scratch() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let mut closed = Vec::new();
        for _ in 0..2 {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).view = View::Label("label".into());
            h.open_commits("/repo");
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseCommits);
            closed.push((
                h.stoat.active_workspace().panes.split_pane_ids().len(),
                h.stoat.current_view(),
            ));
        }

        assert_eq!(
            closed,
            [(1, Some("file")), (1, Some("file"))],
            "a list over a label closes its pane, and the last pane gets a scratch editor"
        );
    }

    #[test]
    fn a_walk_over_a_list_that_covers_a_shell_drops_the_list() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let shell = {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            let pane = ws.panes.pane_mut(focus);
            let covering = pane
                .prev_view
                .take()
                .expect("the view the terminal replaced");
            let View::Terminal(shell) = std::mem::replace(&mut pane.view, covering) else {
                panic!("the terminal action shows a terminal");
            };
            pane.prev_view = Some(View::Terminal(shell));
            shell
        };

        walk_from_the_list(&mut h);

        let ws = h.stoat.active_workspace();
        let behind = &ws.panes.pane(ws.panes.focus()).prev_view;
        assert_eq!(
            (
                ws.commit_lists.len(),
                matches!(behind, Some(View::Terminal(id)) if *id == shell)
            ),
            (0, true),
            "the pane keeps its shell behind the walk, and the list it has no slot for goes"
        );
    }

    #[test]
    fn closing_a_list_pane_keeps_a_shell_another_pane_shows() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let ws = h.stoat.active_workspace();
        let View::Terminal(shell) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the split shows the shell too");
        };
        h.open_commits("/repo");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.commit_lists.len(), ws.terms.contains_key(shell)),
            (0, true),
            "the list closes with its pane, and the shell another pane shows lives on"
        );
    }

    #[test]
    fn closing_a_pane_under_a_walk_drops_the_list_beneath() {
        list_beneath_a_walk_closes_with(&stoat_action::ClosePane);
    }

    #[test]
    fn closing_a_tab_under_a_walk_drops_the_list_beneath() {
        list_beneath_a_walk_closes_with(&stoat_action::CloseTab);
    }

    #[test]
    fn a_shell_over_the_list_gives_the_list_back_when_it_exits() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the terminal action shows a terminal");
        };

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited { term_id });

        assert_eq!(
            h.stoat.current_view(),
            Some("commits"),
            "the list comes back when the shell over it exits"
        );
    }

    #[test]
    fn splitting_a_list_pane_gives_the_new_pane_an_editor() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");
        let list = h
            .stoat
            .active_workspace()
            .focused_commits_id()
            .expect("the list pane");

        h.type_keys("space a s");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.current_view(),
                ws.commit_lists.len(),
                ws.commit_lists[list].mode.as_str()
            ),
            (Some("file"), 1, "normal"),
            "the split opens an editor beside the list, and the list leaves the leader mode"
        );
    }

    #[test]
    fn an_unfocused_list_dims_and_names_itself() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.open_commits("/repo");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::TogglePaneWiden);
        let list_pane = h.stoat.active_workspace().panes.focus();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);

        let mut first_rows = Vec::new();
        for dim in [0.0, 0.6] {
            h.stoat.settings.ui_inactive_dim = Some(dim);
            h.snapshot();
            let area = h.stoat.active_workspace().panes.pane(list_pane).area;
            let buf = h.rendered_buffer();
            let row: Vec<_> = (area.x..area.right())
                .map(|x| buf[(x, area.y)].style())
                .collect();
            first_rows.push(row);
        }
        let area = h.stoat.active_workspace().panes.pane(list_pane).area;
        let status: String = h
            .rendered_text()
            .lines()
            .nth(usize::from(area.bottom() - 1))
            .expect("the list's status row")
            .chars()
            .skip(usize::from(area.x))
            .collect();

        assert_eq!(
            (first_rows[0] != first_rows[1], status.contains("commits")),
            (true, true),
            "an unfocused list dims and names itself in its status row:\n{status}"
        );
    }

    #[test]
    fn a_respawn_leaves_a_live_list_alone() {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        h.open_commits("/repo");
        let live = h.stoat.active_workspace().focused_commits_id();

        super::respawn_commits_panes(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.focused_commits_id(), ws.commit_lists.len()),
            (live, 1),
            "the pane keeps its list, and no second list starts"
        );
    }

    #[test]
    fn a_respawn_outside_a_repository_leaves_a_closed_label() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.git_root = "/nowhere".into();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).view = View::Commits(CommitListId::default());

        super::respawn_commits_panes(&mut h.stoat);

        let label = match &h.stoat.active_workspace().panes.pane(focus).view {
            View::Label(label) => Some(label.clone()),
            _ => None,
        };
        assert_eq!(
            label.as_deref(),
            Some("Commits (closed)"),
            "a pane outside a repository shows that its list closed"
        );
    }

    /// Walk from a list in one of two split panes of a second tab, dispatch
    /// `action`, and assert that the list beneath the walk goes with it.
    fn list_beneath_a_walk_closes_with(action: &dyn stoat_action::Action) {
        let mut h = Stoat::test();
        seed_two_commits_on_main(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        walk_from_the_list(&mut h);
        let open = h.stoat.active_workspace().commit_lists.len();

        crate::action_handlers::dispatch(&mut h.stoat, action);

        assert_eq!(
            (open, h.stoat.active_workspace().commit_lists.len()),
            (1, 0),
            "the list beneath the walk closes with what held it"
        );
    }

    /// Open the list over `/repo` and walk the selected commit, which puts the
    /// commit's diff in front of the list.
    fn walk_from_the_list(h: &mut crate::test_harness::TestHarness) {
        h.open_commits("/repo");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitsOpenReview);
        h.settle();
    }

    /// Seed `/repo` with two commits and HEAD on `main` at the newer one, so a
    /// walk opened from the list has a branch to return to.
    fn seed_two_commits_on_main(h: &mut crate::test_harness::TestHarness) {
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "b2c3d4e5")
            .set_head_branch("main");
        h.fake_fs().insert_file("/repo/a.rs", b"2\n");
    }

    /// Returns the sha that the open commits list selects.
    fn selected(h: &crate::test_harness::TestHarness) -> String {
        h.stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state")
            .selected_sha()
            .expect("selection")
            .to_string()
    }

    #[test]
    fn the_arrows_step_the_commits_selection() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.open_commits("/repo");

        let top = selected(&h);

        h.type_keys("down");
        let after_down = selected(&h);
        h.type_keys("up");

        assert_eq!(
            (after_down, selected(&h)),
            ("a1b2c3d4".to_string(), top),
            "down steps onto the next row and up comes back"
        );
    }

    #[test]
    fn a_count_steps_the_commits_selection_and_g_selects_the_nth() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
                ("c3d4e5f6", "three", &[("a.rs", "3\n")]),
                ("d4e5f6a7", "four", &[("a.rs", "4\n")]),
            ],
        );
        h.open_commits("/repo");

        let walked: Vec<String> = ["2 j", "9 j", "9 k", "3 G", "G"]
            .into_iter()
            .map(|keys| {
                h.type_keys(keys);
                selected(&h)
            })
            .collect();

        assert_eq!(
            walked,
            ["b2c3d4e5", "a1b2c3d4", "d4e5f6a7", "b2c3d4e5", "a1b2c3d4"],
            "a count steps and clamps, a count before G selects that row, and a bare G selects the last"
        );
    }

    #[test]
    fn the_commits_status_bar_paints_the_pending_count() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.open_commits("/repo");

        let status_row = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let ws = h.stoat.active_workspace();
            let row = ws.panes.pane(ws.panes.focus()).area.bottom() - 1;
            h.rendered_text()
                .lines()
                .nth(usize::from(row))
                .expect("the pane's status row")
                .to_string()
        };
        let before = status_row(&mut h);
        h.type_keys("4 0");
        let counted = status_row(&mut h);
        h.type_keys("j");
        let after = status_row(&mut h);

        assert_eq!(
            [&before, &counted, &after].map(|row| row.ends_with(" 40")),
            [false, true, false],
            "the count holds the bar's right edge until j consumes it:\n{counted}"
        );
    }

    /// Rows scrolled past do not each get a build of their own.
    ///
    /// A dropped task keeps running on the blocking pool, so one build per
    /// keystroke would put every row passed through ahead of the row the
    /// selection stops on, each waiting on the repo lock. Holding the count at
    /// one costs nothing, since the pump comes back for the current selection
    /// the moment the running build lands.
    #[test]
    fn stepping_the_selection_twice_leaves_one_build_running() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
                ("c3d4e5f6", "three", &[("a.rs", "3\n")]),
            ],
        );
        h.open_commits("/repo");

        // Two moves with nothing polled between them, which is what holding the
        // key down does.
        let moved = h
            .stoat
            .active_workspace_mut()
            .focused_commits_mut()
            .expect("commits state")
            .move_down(1);
        assert!(moved, "the list has a row to step onto");
        let list = h
            .stoat
            .active_workspace()
            .focused_commits_id()
            .expect("the list pane");
        super::ensure_selected_preview(&mut h.stoat, list);
        let first = h
            .stoat
            .active_workspace()
            .focused_commits()
            .and_then(|s| s.pending_preview.as_ref())
            .map(|p| p.sha.clone());
        assert!(
            first.is_some(),
            "the first step has to start a build, or there is nothing to block",
        );

        h.stoat
            .active_workspace_mut()
            .focused_commits_mut()
            .expect("commits state")
            .move_down(1);
        super::ensure_selected_preview(&mut h.stoat, list);

        assert_eq!(
            h.stoat
                .active_workspace()
                .focused_commits()
                .and_then(|s| s.pending_preview.as_ref())
                .map(|p| p.sha.clone()),
            first,
            "the second step waits on the build the first one started",
        );

        // Whatever it started on, it ends up showing the row it stopped at.
        h.settle();
        let state = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state");
        let selected = state.selected_sha().expect("selection").to_string();
        assert!(
            matches!(state.preview_sessions.get(&selected), Preview::Built(_)),
            "the preview for the row the selection came to rest on is cached",
        );
    }

    /// The log pump polls with a noop waker, so without a wake at completion a
    /// page that lands after the last input event stays invisible until some
    /// unrelated event drives the pumps.
    #[test]
    fn a_log_page_wakes_the_run_loop_when_it_lands() {
        use futures::FutureExt;

        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "b2c3d4e5",
                    "chore: tweak a",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n")],
                ),
            ],
        );
        h.open_commits("/repo");

        // Opening the view wakes the loop too. Drain that permit against an Arc
        // clone, so the observer never borrows `h` across settle, leaving the
        // refresh's own load as the only wake to observe. Notify holds at most
        // one permit, so a single drain clears it.
        let redraw = h.stoat.redraw_notify.clone();
        let _ = redraw.notified().now_or_never();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitsRefresh);
        h.settle();

        let notified = redraw.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "the refreshed log page should wake the loop so the list paints \
             without waiting for the next keystroke",
        );
    }
}
