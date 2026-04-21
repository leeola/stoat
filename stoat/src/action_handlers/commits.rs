use crate::{
    app::{Stoat, UpdateEffect},
    review_session::{ReviewSession, ReviewSource},
};
use std::{path::Path, sync::Arc};

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

    let mut state = CommitListState::new(workdir);
    state.pending_load = Some(spawn_commit_log_load(
        &stoat.executor,
        repo,
        None,
        COMMITS_INITIAL_PAGE,
    ));

    stoat.active_workspace_mut().commits = Some(state);
    stoat.mode = "commits".to_string();
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
    stoat.mode = "normal".to_string();
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
    let task = spawn_commit_log_load(&stoat.executor, repo, None, COMMITS_INITIAL_PAGE);
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
    let task = spawn_commit_log_load(&stoat.executor, repo, Some(last_sha), COMMITS_INITIAL_PAGE);
    if let Some(state) = stoat.active_workspace_mut().commits.as_mut() {
        state.pending_load = Some(task);
    }
}

/// Spawn a background preview build for the current selection if one is
/// not already cached or in flight. Also populates the file-change
/// summary synchronously (cheap: one tree-diff).
fn ensure_selected_preview(stoat: &mut Stoat) {
    let Some(state) = stoat.active_workspace().commits.as_ref() else {
        return;
    };
    let Some(sha) = state.selected_sha().map(str::to_string) else {
        return;
    };
    let workdir = state.workdir.clone();
    let need_summary = !state.summaries.contains_key(&sha);
    let need_preview = !state.preview_sessions.contains_key(&sha)
        && state.pending_preview.as_ref().is_none_or(|p| p.sha != sha);

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

fn spawn_commit_log_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    after: Option<String>,
    limit: usize,
) -> stoat_scheduler::Task<Vec<crate::host::CommitInfo>> {
    executor.spawn(async move { repo.log_commits(after.as_deref(), limit) })
}

fn spawn_commit_preview_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    workdir: std::path::PathBuf,
    sha: String,
    language_registry: Arc<stoat_language::LanguageRegistry>,
) -> stoat_scheduler::Task<Option<ReviewSession>> {
    executor.spawn(async move {
        let new_tree = repo.commit_tree(&sha)?;
        let base_tree = match repo.parent_sha(&sha) {
            Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        };
        let source = ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.clone(),
        };
        build_session_from_trees_pure(source, &workdir, &base_tree, &new_tree, &language_registry)
    })
}

/// Stateless variant of `build_session_from_trees` usable from async
/// preview tasks that do not hold a `&Stoat`.
fn build_session_from_trees_pure(
    source: ReviewSource,
    workdir: &Path,
    base_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
    new_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
    language_registry: &stoat_language::LanguageRegistry,
) -> Option<ReviewSession> {
    let mut paths: std::collections::BTreeSet<&Path> = std::collections::BTreeSet::new();
    for p in base_tree.keys() {
        paths.insert(p.as_path());
    }
    for p in new_tree.keys() {
        paths.insert(p.as_path());
    }
    if paths.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(source);
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = language_registry.for_path(&abs);
        session.add_file(
            abs,
            rel.display().to_string(),
            lang,
            Arc::new(base),
            Arc::new(buffer),
        );
    }
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}
