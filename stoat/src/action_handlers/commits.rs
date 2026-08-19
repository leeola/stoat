use crate::{
    app::{Stoat, UpdateEffect},
    review_session::{ReviewSession, ReviewSource},
};
use std::sync::Arc;

const COMMITS_INITIAL_PAGE: usize = 64;
const COMMITS_PREFETCH_GAP: usize = 8;
const COMMITS_PAGE_STEP: usize = 16;

#[derive(Copy, Clone, Debug)]
pub(super) enum CommitStep {
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    First,
    Last,
}

pub(super) fn open_commits(stoat: &mut Stoat) -> UpdateEffect {
    use crate::commit_list::CommitListState;

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        tracing::warn!("open_commits: not inside a git repository");
        return UpdateEffect::None;
    };
    let Some(workdir) = repo.workdir() else {
        tracing::warn!("open_commits: git repo has no workdir");
        return UpdateEffect::None;
    };

    // The commits screen ranks below the diff screen, so a list installed over
    // an open diff would never paint. Leaving the diff first is what the user
    // otherwise does by hand. The helper gates itself, so a plain pane, widened
    // or not, is untouched.
    super::review::exit_diff_view(stoat);

    let mut state = CommitListState::new(workdir);
    state.pending_load = Some(spawn_commit_log_load(
        &stoat.executor,
        repo,
        None,
        COMMITS_INITIAL_PAGE,
        stoat.redraw_notify.clone(),
    ));

    stoat.active_workspace_mut().commits = Some(state);
    drain_commits_tasks(stoat);
    ensure_selected_preview(stoat);
    drain_commits_tasks(stoat);
    UpdateEffect::Redraw
}

pub(super) fn close_commits(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    if ws.commits.take().is_none() {
        return UpdateEffect::None;
    }
    stoat.set_focused_mode("normal".to_string());
    UpdateEffect::Redraw
}

pub(super) fn commits_step(stoat: &mut Stoat, step: CommitStep) -> UpdateEffect {
    let moved = {
        let Some(state) = stoat.active_workspace_mut().commits.as_mut() else {
            return UpdateEffect::None;
        };
        let moved = match step {
            CommitStep::Up(n) => state.move_up(n),
            CommitStep::Down(n) => state.move_down(n),
            CommitStep::PageUp => state.move_up(COMMITS_PAGE_STEP),
            CommitStep::PageDown => state.move_down(COMMITS_PAGE_STEP),
            CommitStep::First => state.move_to_first(),
            CommitStep::Last => state.move_to_last(),
        };
        let height = state.viewport_rows;
        state.ensure_selected_visible(height);
        moved
    };
    if !moved {
        return UpdateEffect::None;
    }
    maybe_spawn_next_page(stoat);
    ensure_selected_preview(stoat);
    drain_commits_tasks(stoat);
    UpdateEffect::Redraw
}

pub(super) fn commits_refresh(stoat: &mut Stoat) -> UpdateEffect {
    let Some(git_root) = stoat
        .active_workspace()
        .commits
        .as_ref()
        .map(|s| s.workdir.clone())
    else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat.git_host.discover(&git_root) else {
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
    if let Some(state) = ws.commits.as_mut() {
        state.commits.clear();
        state.reached_end = false;
        state.selected = 0;
        state.scroll_top = 0;
        state.summaries.clear();
        state.preview_sessions.clear();
        state.pending_preview = None;
        state.requested_preview = None;
        state.pending_load = Some(task);
    }
    drain_commits_tasks(stoat);
    ensure_selected_preview(stoat);
    drain_commits_tasks(stoat);
    UpdateEffect::Redraw
}

/// Kick off another page load when the cursor is approaching the tail of
/// the loaded window. No-op when a load is already in flight or the walk
/// has hit a root commit.
fn maybe_spawn_next_page(stoat: &mut Stoat) {
    let Some(state) = stoat.active_workspace().commits.as_ref() else {
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
    let workdir = state.workdir.clone();
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return;
    };
    let task = spawn_commit_log_load(
        &stoat.executor,
        repo,
        Some(last_sha),
        COMMITS_INITIAL_PAGE,
        stoat.redraw_notify.clone(),
    );
    if let Some(state) = stoat.active_workspace_mut().commits.as_mut() {
        state.pending_load = Some(task);
    }
}

/// Spawn a background preview build for the current selection if one is
/// not already cached, and no build is in flight for any commit. Also
/// populates the file-change summary synchronously (cheap: one tree-diff).
///
/// Only one build runs at a time. Dropping a [`stoat_scheduler::Task`] leaves
/// the blocking pool running the closure regardless, so spawning per row would
/// stack a build for every row scrolled past ahead of the row that matters.
/// [`pump_commits`] returns here once the running build lands, which is what
/// carries the selection's latest position through.
fn ensure_selected_preview(stoat: &mut Stoat) {
    let Some(state) = stoat.active_workspace_mut().commits.as_mut() else {
        return;
    };
    let Some(sha) = state.selected_sha().map(str::to_string) else {
        return;
    };
    let workdir = state.workdir.clone();
    let need_summary = !state.summaries.contains_key(&sha);
    let need_preview = !state.preview_sessions.mark_used(&sha) && state.pending_preview.is_none();

    if !need_summary && !need_preview {
        return;
    }
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return;
    };

    let summary = if need_summary {
        Some(repo.commit_file_changes(&sha))
    } else {
        None
    };
    let preview_task = if need_preview {
        let language_registry = stoat.language_registry.clone();
        Some(spawn_commit_preview_load(
            &stoat.executor,
            repo.clone(),
            workdir.clone(),
            sha.clone(),
            language_registry,
            stoat.redraw_notify.clone(),
        ))
    } else {
        None
    };

    let ws = stoat.active_workspace_mut();
    if let Some(state) = ws.commits.as_mut() {
        if let Some(changes) = summary {
            state.summaries.insert(sha.clone(), changes);
        }
        if let Some(task) = preview_task {
            state.requested_preview = Some(sha.clone());
            state.pending_preview = Some(crate::commit_list::PendingPreview { sha, task });
        }
    }
}

/// Poll both commit-list pending tasks to completion-or-pending. Called
/// after every action handler that touches the commit list so tests
/// which settle the scheduler see consistent state on the next render.
fn drain_commits_tasks(stoat: &mut Stoat) {
    let Some(state) = stoat.active_workspace_mut().commits.as_mut() else {
        return;
    };
    state.poll_pending_load();
    state.poll_pending_preview();
}

/// Pull completed commit-list tasks into state and spawn any follow-up
/// work unlocked by those completions (e.g. after the first log page
/// lands we can request a preview for the selected commit). Returns true
/// when any task landed or a new task was spawned.
///
/// Called at the top of every `Stoat::render` tick so the UI reflects
/// settled state without requiring navigation input. Also called in the
/// test harness's `settle` loop so `assert_snapshot` sees terminal state
/// regardless of how many scheduler ticks the work needs.
pub(crate) fn pump_commits(stoat: &mut Stoat) -> bool {
    let landed = {
        let Some(state) = stoat.active_workspace_mut().commits.as_mut() else {
            return false;
        };
        let a = state.poll_pending_load();
        let b = state.poll_pending_preview();
        a || b
    };
    let spawned_before = {
        let Some(state) = stoat.active_workspace().commits.as_ref() else {
            return landed;
        };
        state.pending_load.is_some() || state.pending_preview.is_some()
    };
    ensure_selected_preview(stoat);
    maybe_spawn_next_page(stoat);
    let spawned_after = {
        let Some(state) = stoat.active_workspace().commits.as_ref() else {
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

/// Spawn the blocking diff build for `sha`, waking the run loop through `redraw`
/// when it lands.
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
) -> stoat_scheduler::Task<Option<ReviewSession>> {
    executor.spawn_blocking(move || {
        let parent = repo.parent_sha(&sha);
        let built = match super::review::changed_or_whole(&*repo, parent.as_deref(), &sha) {
            Some(changes) => {
                let source = ReviewSource::Commit {
                    workdir: workdir.clone(),
                    sha: sha.clone(),
                };
                super::review::build_session_from_changes(
                    &language_registry,
                    source,
                    &workdir,
                    changes,
                )
            },
            None => None,
        };
        redraw.notify_one();
        built
    })
}

#[cfg(test)]
mod tests {
    use crate::app::Stoat;

    /// The commits screen ranks below the diff screen, so a list opened over an
    /// open diff would install its state and never paint. Opening the list
    /// exits the diff first, which is the close-then-see the user had to do by
    /// hand.
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff);
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
        let diff_view = h
            .stoat
            .focused_editor_ids()
            .and_then(|(id, _)| h.stoat.active_workspace().editors.get(id))
            .is_some_and(|editor| editor.diff_view);
        assert_eq!(
            (
                h.stoat.current_view(),
                diff_view,
                latched,
                widened.is_some()
            ),
            (Some("commits"), false, false, false),
            "the list paints, and the diff left no flag, latch, or widen behind"
        );
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

        let selected = |h: &crate::test_harness::TestHarness| {
            h.stoat
                .active_workspace()
                .commits
                .as_ref()
                .expect("commits state")
                .selected_sha()
                .expect("selection")
                .to_string()
        };
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
            .commits
            .as_mut()
            .expect("commits state")
            .move_down(1);
        assert!(moved, "the list has a row to step onto");
        super::ensure_selected_preview(&mut h.stoat);
        let first = h
            .stoat
            .active_workspace()
            .commits
            .as_ref()
            .and_then(|s| s.pending_preview.as_ref())
            .map(|p| p.sha.clone());
        assert!(
            first.is_some(),
            "the first step has to start a build, or there is nothing to block",
        );

        h.stoat
            .active_workspace_mut()
            .commits
            .as_mut()
            .expect("commits state")
            .move_down(1);
        super::ensure_selected_preview(&mut h.stoat);

        assert_eq!(
            h.stoat
                .active_workspace()
                .commits
                .as_ref()
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
            .commits
            .as_ref()
            .expect("commits state");
        let selected = state.selected_sha().expect("selection").to_string();
        assert!(
            state.preview_sessions.get(&selected).is_some(),
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
