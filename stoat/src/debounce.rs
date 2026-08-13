//! The debounce arms and drains that turn bursts of filesystem noise into work.
//!
//! A watcher reports every write a build makes, an editor's save reports several
//! events for one file, and a checkout reports thousands at once. Reacting to
//! each one directly would reindex, re-diff, and re-search far more than the user
//! changed, so each consumer here collects paths into a pending set and arms a
//! timer. The drains run from the app's update loop and hand over whatever the
//! window collected.
//!
//! Every function takes the app by reference and reads its channel, task, and
//! pending-set fields directly, which is the crate's shape for a subsystem that
//! drives the app rather than living inside it.

use crate::{
    action_handlers,
    app::Stoat,
    host::{FsEventKind, GitRepo},
    review_session::ReviewSource,
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{ReviewExternalEdit, ReviewRefresh};

/// Quiet window after the last filesystem-watch event for a path
/// before [`ReviewExternalEdit`] dispatches. Mirrors
/// [`crate::lsp::sync::LSP_DID_CHANGE_DEBOUNCE`] so a
/// formatter-on-save burst (or an agent edit chain) collapses
/// into one diff rebuild rather than three.
pub(crate) const REVIEW_EXTERNAL_EDIT_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(50);

/// Quiet window after the last parse of a buffer before its symbols are
/// extracted again.
///
/// An extract reads the whole file and its drain rebuilds the project's
/// adjacency, so a burst of keystrokes must collapse into one rather than
/// queue an extract each. Far longer than
/// [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`], which is sized for a
/// formatter-on-save burst. Typing pauses less often than that, and nothing
/// reads the symbol index between keystrokes.
pub(crate) const INDEX_EDIT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// External-change paths [`drain_pending_index_edits`] hands to the
/// blocking pool in one pass.
///
/// Each costs only a spawn on the run loop, the read and the parse having moved
/// into the job. What the cap paces is the pool: a checkout naming thousands of
/// files queues them over several windows rather than in one turn.
const INDEX_EXTERNAL_DRAIN_CAP: usize = 256;

/// Directory verdicts [`Stoat::ignored_dir_cache`] holds before dropping the
/// lot. Far above the directory count of any one build, so the bound is a
/// backstop against a pathological tree rather than a working limit.
const IGNORED_DIR_CACHE_MAX: usize = 8192;

/// Quiet window after the last code-search keystroke before the blocking
/// workspace scan spawns, so a burst of typing re-scans once rather than per
/// character.
pub(crate) const CODE_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Longer debounce for AST search, whose scan parses every same-language file
/// per query and so costs more than a regex sweep.
pub(crate) const CODE_SEARCH_AST_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(300);

/// Drain queued [`crate::host::FsWatchEvent`]s from the active
/// [`FsWatchHost`]. Each event arms (or resets) a 50ms
/// per-path debounce via
/// [`arm_review_external_edit_debounce`] when a review
/// session is active; the dispatch itself lands later from
/// [`drain_pending_external_edits`] once the timer
/// fires. Cap matches [`Stoat::drain_lsp_notifications`] so a
/// pathological burst can't starve the event loop.
pub(crate) fn drain_fs_watch_events(stoat: &mut Stoat) {
    let host = stoat.fs_watch_host.clone();
    let mut events: Vec<(PathBuf, FsEventKind)> = Vec::new();
    for _ in 0..256 {
        let Some(event) = host.try_recv() else {
            break;
        };
        tracing::trace!(
            target: "stoat::app",
            path = %event.path.display(),
            kind = ?event.kind,
            "fs watch event observed",
        );
        events.push((event.path, event.kind));
    }

    if events.is_empty() {
        return;
    }
    // `review.follow` gates every automatic refresh below. A manual `r`
    // (ReviewRefresh) dispatches through a separate path and still works.
    let review_active =
        stoat.active_workspace().review.is_some() && stoat.settings.review_follow.unwrap_or(true);
    // With no review open, keep the diff cache warm incrementally instead,
    // gated the same as the full background warm.
    let precompute = stoat.active_workspace().review.is_none()
        && stoat.diff_warm_auto
        && stoat.settings.review_precompute.unwrap_or(true);
    let git_root = stoat.active_workspace().git_root.clone();
    let git_dir = git_root.join(".git");
    let mut repo: Option<Option<Arc<dyn GitRepo>>> = None;
    for (path, kind) in events {
        let in_git_dir = path.starts_with(&git_dir);
        let is_gitignore = path.file_name() == Some(OsStr::new(".gitignore"));

        // Invalidate before reading any verdict, so a .gitignore edit drained
        // alongside later paths in the same batch cannot leave them resolving
        // against the rules it just replaced.
        if in_git_dir || is_gitignore {
            stoat.ignored_dir_cache.clear();
        }

        // The rules deciding which files a walk yields just moved, so the
        // cached list was built under different ones.
        if is_gitignore {
            stoat.finder_path_epoch += 1;
        }

        // A whole ignored directory is build churn, so none of the three arms
        // below want it. The initial index walk filters these out too, so
        // reindexing one here would put generated files in the code graph
        // that a rebuild drops.
        if !in_git_dir && parent_dir_ignored(stoat, &path, &git_root, &mut repo) {
            continue;
        }

        // Past the ignored-directory filter, so what is left is a real
        // source-tree change rather than build churn. Anything that adds or
        // removes a path makes a cached walk list wrong, and a plain content
        // edit does not.
        if !in_git_dir
            && path.starts_with(&git_root)
            && matches!(
                kind,
                FsEventKind::Created | FsEventKind::Removed | FsEventKind::Renamed
            )
        {
            stoat.finder_path_epoch += 1;

            // Watches are per directory, so one created after startup is
            // invisible until it gets its own. The stat is affordable here
            // because the ignored-directory filter above already dropped
            // build churn, leaving creates in the source tree, which are
            // rare.
            if kind == FsEventKind::Created
                && stoat
                    .fs_host
                    .metadata(&path)
                    .ok()
                    .flatten()
                    .is_some_and(|meta| meta.is_dir)
            {
                let _ = stoat.fs_watch_host.watch(&path);
            }
        }

        if in_git_dir {
            // A .git write (a commit, reset, rebase step, or branch switch)
            // moved HEAD and staled every diff base, so it refreshes through
            // the shared debounce whatever the session state is. Its drain
            // refreshes an open review, clears the diff_warmed flag, and
            // stales every open buffer's diff map.
            arm_review_git_refresh_debounce(stoat);
        } else if review_active {
            let in_session = stoat
                .active_workspace()
                .review
                .as_ref()
                .is_some_and(|s| s.files.iter().any(|f| f.path == path));
            if in_session {
                // A tracked file keeps the per-path debounce, which scrolls
                // the review to the edited chunk when the refresh lands.
                arm_review_external_edit_debounce(stoat, path.clone());
            } else if path.starts_with(&git_root) {
                // A change to a working-tree file not yet in the session
                // pulls it in on the next refresh, unless gitignored so
                // build churn such as target/ cannot thrash the rescan.
                let repo = repo.get_or_insert_with(|| stoat.git_host.discover(&git_root));
                if !repo.as_ref().is_some_and(|r| r.is_path_ignored(&path)) {
                    arm_review_git_refresh_debounce(stoat);
                }
            }
        } else if precompute && path.starts_with(&git_root) {
            // An edited working-tree file warms its own diff, unless
            // gitignored so build churn cannot thrash the recompute.
            let repo = repo.get_or_insert_with(|| stoat.git_host.discover(&git_root));
            if !repo.as_ref().is_some_and(|r| r.is_path_ignored(&path)) {
                arm_diff_warm_file_debounce(stoat, path.clone());
            }
        }
        if path.starts_with(&git_root) && stoat.language_registry.for_path(&path).is_some() {
            arm_index_external_edit_debounce(stoat, path);
        }
    }
}

/// Whether `path` sits in a gitignored directory, answering from
/// [`Stoat::ignored_dir_cache`] when the directory has been asked about
/// before.
///
/// Asks about the parent rather than `path` itself, which is what makes the
/// answer reusable, since one query then covers every file a build writes
/// into that directory. A file individually ignored inside a clean directory
/// is not caught here, so the callers that care keep their own per-file
/// check.
///
/// `repo` memoizes the repository discovery across one drain batch. Paths
/// outside `git_root` have no verdict and report false.
fn parent_dir_ignored(
    stoat: &mut Stoat,
    path: &Path,
    git_root: &Path,
    repo: &mut Option<Option<Arc<dyn GitRepo>>>,
) -> bool {
    if !path.starts_with(git_root) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Some(ignored) = stoat.ignored_dir_cache.get(parent) {
        return *ignored;
    }

    let git_host = stoat.git_host.clone();
    let repo = repo.get_or_insert_with(|| git_host.discover(git_root));
    let ignored = repo.as_ref().is_some_and(|r| r.is_path_ignored(parent));

    // A bound rather than an eviction policy, because the working set here is
    // the directories one build touches. Dropping all of it costs at most one
    // query per directory the next batch revisits.
    if stoat.ignored_dir_cache.len() >= IGNORED_DIR_CACHE_MAX {
        stoat.ignored_dir_cache.clear();
    }
    stoat
        .ignored_dir_cache
        .insert(parent.to_path_buf(), ignored);
    ignored
}

/// Schedule a debounced [`ReviewExternalEdit`] dispatch for
/// `path`. Inserting into [`Stoat::review_pending_external_edits`]
/// drops any prior task for the same path, which cancels the
/// spawned future at its [`Executor::timer`] await; only the
/// most recent burst event proceeds. The spawned task forwards
/// `path` on [`Stoat::review_external_edit_tx`] when its
/// [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses; the main
/// loop drains the channel via
/// [`drain_pending_external_edits`] and dispatches the
/// action there because async tasks cannot mutate `Stoat`.
fn arm_review_external_edit_debounce(stoat: &mut Stoat, path: PathBuf) {
    let executor = stoat.executor.clone();
    let tx = stoat.review_external_edit_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    let path_for_send = path.clone();
    let task = stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
        let _ = tx.send(path_for_send).await;
    });
    stoat.review_pending_external_edits.insert(path, task);
}

/// Point the code-search scan at `query`, debounced.
///
/// Drops any in-flight scan and arms a timer that forwards `query` on
/// [`Stoat::code_search_query_tx`] after [`CODE_SEARCH_DEBOUNCE`], so a burst
/// of typing spawns one scan. An empty or invalid pattern cancels without
/// arming, since an empty regex would otherwise match every position.
pub(crate) fn set_code_search_query(stoat: &mut Stoat, query: String) {
    stoat.pending_code_search = None;
    let Some(finder) = stoat.code_search.as_ref() else {
        stoat.code_search_debounce = None;
        return;
    };
    let is_ast = matches!(finder.mode, crate::code_search::SearchMode::Ast);
    let valid = finder.pattern_valid(&query);

    // AST mode surfaces a non-empty, unparseable query as a placeholder;
    // regex validity is silent, since an invalid regex just clears the list.
    if is_ast && let Some(finder) = stoat.code_search.as_mut() {
        finder.invalid_pattern = !query.is_empty() && !valid;
    }
    if !valid {
        stoat.code_search_debounce = None;
        return;
    }

    let debounce = if is_ast {
        CODE_SEARCH_AST_DEBOUNCE
    } else {
        CODE_SEARCH_DEBOUNCE
    };
    let executor = stoat.executor.clone();
    let tx = stoat.code_search_query_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.code_search_debounce = Some(stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(debounce).await;
        let _ = tx.send(query).await;
    }));
}

/// Drain the debounced code-search query and spawn its scan.
///
/// The debounce timer forwards the settled query here. Spawning the blocking
/// walk on the main loop lets its batches stream into the open finder.
pub(crate) fn drain_pending_code_search(stoat: &mut Stoat) -> bool {
    let mut progressed = false;
    for _ in 0..256 {
        let Ok(query) = stoat.code_search_query_rx.try_recv() else {
            break;
        };
        if stoat.code_search.is_none() {
            continue;
        }
        let git_root = stoat.active_workspace().git_root.clone();
        if let Some(pending) =
            action_handlers::code_search::spawn_code_search(stoat, git_root, &query)
        {
            stoat.pending_code_search = Some(pending);
            progressed = true;
        }
    }
    progressed
}

/// Schedule a debounced whole-session [`ReviewRefresh`] after a git-state
/// change under `<git_root>/.git`.
///
/// The debounce is single-slot rather than per-path, so re-arming drops the
/// prior task, which cancels its future at the [`Executor::timer`] await. A
/// burst of `.git` writes from one commit then fires a single refresh once
/// the [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses. The main loop drains
/// it via [`drain_pending_git_refresh`], since async tasks cannot
/// mutate `Stoat`.
fn arm_review_git_refresh_debounce(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let tx = stoat.review_git_refresh_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
        let _ = tx.send(()).await;
    });
    stoat.review_pending_git_refresh = Some(task);
}

/// Schedule a debounced single-file diff warm for `path` edited while review
/// is closed. Mirrors [`arm_review_external_edit_debounce`]: inserting
/// into [`Stoat::pending_diff_warm_file`] drops any prior task for the same
/// path, so only the latest burst event warms once its
/// [`REVIEW_EXTERNAL_EDIT_DEBOUNCE`] window elapses. The spawned task
/// forwards `path` on [`Stoat::diff_warm_file_tx`], drained by
/// [`drain_pending_diff_warm_files`], which spawns the warm off-thread.
fn arm_diff_warm_file_debounce(stoat: &mut Stoat, path: PathBuf) {
    let executor = stoat.executor.clone();
    let tx = stoat.diff_warm_file_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    let path_for_send = path.clone();
    let task = stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
        let _ = tx.send(path_for_send).await;
    });
    stoat.pending_diff_warm_file.insert(path, task);
}

/// Drain every path the per-path debounce tasks have pushed onto
/// [`Stoat::review_external_edit_tx`] since the last call. Each
/// path becomes one [`ReviewExternalEdit`] dispatch when a review
/// session is active; otherwise the path is dropped. Returns
/// `true` if any dispatch fired so the test harness's settle
/// loop can re-iterate. Cap matches
/// [`drain_fs_watch_events`].
pub(crate) fn drain_pending_external_edits(stoat: &mut Stoat) -> bool {
    let mut progressed = false;
    for _ in 0..256 {
        let Ok(path) = stoat.review_external_edit_rx.try_recv() else {
            break;
        };
        stoat.review_pending_external_edits.remove(&path);
        if stoat.active_workspace().review.is_some() {
            action_handlers::dispatch(stoat, &ReviewExternalEdit { path });
            progressed = true;
        }
    }
    progressed
}

/// Drain the git-refresh debounce marker and refresh the review when one
/// fired since the last call.
///
/// Only a [`ReviewSource::WorkingTree`] session refreshes. Commit and
/// commit-range sources are fixed snapshots the rebase edit-pause review
/// relies on not churning, and in-memory agent-edit sources are not
/// git-backed. With no working-tree review, a `.git` change instead re-arms
/// the full background warm, since HEAD moved and staled every cached base.
///
/// Either way the drain stales every open buffer's diff map, since those are
/// keyed by buffer version alone and a moved HEAD leaves them describing a
/// base that no longer exists. Gutter marks and the per-file `:diff` view
/// then follow each rebase step instead of freezing at the pre-rebase base.
///
/// Returns `true` if a refresh dispatched so the test harness settle loop
/// re-iterates.
pub(crate) fn drain_pending_git_refresh(stoat: &mut Stoat) -> bool {
    let mut progressed = false;
    for _ in 0..256 {
        let Ok(()) = stoat.review_git_refresh_rx.try_recv() else {
            break;
        };
        stoat.review_pending_git_refresh = None;
        stoat.active_workspace_mut().invalidate_all_diffs();
        // A working-tree review refreshes on any git write. An auto_source
        // session refreshes too even when it currently displays a Commit,
        // so a rebase-fallback view re-decides and follows each rebase step.
        let refreshes = matches!(
            stoat.active_workspace().review.as_ref(),
            Some(s) if matches!(s.source, ReviewSource::WorkingTree { .. }) || s.auto_source
        );
        if refreshes {
            action_handlers::dispatch(stoat, &ReviewRefresh);
            progressed = true;
        } else {
            stoat.active_workspace_mut().diff_warmed = false;
        }
    }
    progressed
}

/// Drain the diff-warm debounce channel, spawning a single-file warm for
/// each path edited while review was closed.
///
/// Mirrors [`drain_pending_external_edits`]. Skips a path when review
/// has since opened -- its own refresh covers the edit -- or when
/// `review.precompute` or the warm auto-gate is off. Returns `true` if a
/// warm spawned so the test harness settle loop re-iterates.
pub(crate) fn drain_pending_diff_warm_files(stoat: &mut Stoat) -> bool {
    let mut progressed = false;
    for _ in 0..256 {
        let Ok(path) = stoat.diff_warm_file_rx.try_recv() else {
            break;
        };
        stoat.pending_diff_warm_file.remove(&path);
        let precompute = stoat.diff_warm_auto && stoat.settings.review_precompute.unwrap_or(true);
        if precompute && stoat.active_workspace().review.is_none() {
            crate::diff_warm::spawn_file_warm(stoat, path);
            progressed = true;
        }
    }
    progressed
}

/// Collect `path` for a debounced reindex after an external change.
///
/// Unlike [`arm_review_external_edit_debounce`], a burst shares one
/// window rather than getting a timer each. The window opens on the first
/// path and covers every path that arrives before it closes, so a checkout
/// costs one task however many files it touches.
fn arm_index_external_edit_debounce(stoat: &mut Stoat, path: PathBuf) {
    let opening = stoat.index_pending_external_edits.is_empty();
    stoat.index_pending_external_edits.insert(path);
    if opening {
        arm_index_external_edit_timer(stoat);
    }
}

/// Start the shared debounce window, replacing any timer already held.
fn arm_index_external_edit_timer(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let tx = stoat.index_external_edit_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.index_external_edit_timer = Some(stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(REVIEW_EXTERNAL_EDIT_DEBOUNCE).await;
        let _ = tx.send(()).await;
    }));
}

/// Reindex the external-change paths whose debounce window has closed.
/// Returns `true` if any path was handled so the harness settle loop
/// re-iterates.
///
/// Takes at most [`INDEX_EXTERNAL_DRAIN_CAP`] paths per pass and re-arms
/// the window on whatever is left, so a checkout's worth of files reaches
/// the blocking pool in paced batches rather than all in one turn.
pub(crate) fn drain_pending_index_edits(stoat: &mut Stoat) -> bool {
    if stoat.index_external_edit_rx.try_recv().is_err() {
        return false;
    }

    let mut pending = std::mem::take(&mut stoat.index_pending_external_edits);
    let batch: Vec<PathBuf> = pending
        .iter()
        .take(INDEX_EXTERNAL_DRAIN_CAP)
        .cloned()
        .collect();
    for path in &batch {
        pending.remove(path);
    }
    stoat.index_pending_external_edits = pending;

    for path in batch {
        reindex_external_path(stoat, path);
    }

    if !stoat.index_pending_external_edits.is_empty() {
        arm_index_external_edit_timer(stoat);
    }
    true
}

/// Reindex a file changed outside the editor.
///
/// A still-present file whose on-disk content matches the graph's
/// recorded hash is skipped, since the editor's own save already
/// indexed it. A changed file is re-extracted from disk. A file that no
/// longer exists is removed from the graph.
///
/// All three decisions are the job's, because telling them apart means
/// reading the file. What stays here is the graph lookup naming what the
/// job expects to find, which costs one map read.
fn reindex_external_path(stoat: &mut Stoat, path: PathBuf) {
    let workspace = stoat.active_workspace;
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(rel_path) = crate::code_index::build::relpath(&git_root, &path) else {
        return;
    };
    let file = crate::code_index::build::file_id(&rel_path);

    let handles = crate::code_index::build::IndexBuild {
        fs: stoat.fs_host.clone(),
        languages: stoat.language_registry.clone(),
        tx: stoat.index_update_tx.clone(),
        redraw: stoat.redraw_notify.clone(),
    };
    let target = crate::code_index::build::ExternalReindex {
        git_root,
        workspace,
        path,
        rel_path,
        file,
        expected: stoat.active_workspace().code_graph.content_hash(file),
    };
    crate::code_index::build::reindex_path(&stoat.executor, handles, target).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    // TEST IMPORTS

    /// A repo with `target/` gitignored and precompute on, so a drained event
    /// reaches both the diff-warm arm and the reindex arm.
    fn ignored_dir_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.fake_git().add_repo("/repo").ignored("target/debug");
        h
    }

    /// A build writing into an ignored directory used to cost a libgit2 query
    /// per file in the review and diff-warm arms, and the reindex arm checked
    /// nothing at all, so generated files entered the code graph.
    #[test]
    fn a_burst_under_an_ignored_dir_arms_nothing() {
        let mut h = ignored_dir_harness();

        for name in ["one.rs", "two.rs", "three.rs"] {
            h.fake_fs_watcher().inject(
                PathBuf::from("/repo/target/debug").join(name),
                FsEventKind::Modified,
            );
        }
        drain_fs_watch_events(&mut h.stoat);

        assert!(
            h.stoat.pending_diff_warm_file.is_empty(),
            "no diff warm is armed for build output"
        );
        assert!(
            h.stoat.index_pending_external_edits.is_empty(),
            "and none of it reaches the code graph"
        );

        h.fake_fs_watcher()
            .inject(Path::new("/repo/src/b.rs"), FsEventKind::Modified);
        drain_fs_watch_events(&mut h.stoat);

        assert_eq!(
            h.stoat.index_pending_external_edits.len(),
            1,
            "a source file beside it still arms the index debounce"
        );
    }

    /// The cached verdict has to die with the rules it came from, or a directory
    /// already asked about keeps its old answer for the rest of the session.
    #[test]
    fn a_gitignore_edit_drops_the_cached_verdict() {
        let mut h = ignored_dir_harness();
        let built = PathBuf::from("/repo/generated/one.rs");

        h.fake_fs_watcher().inject(&built, FsEventKind::Modified);
        drain_fs_watch_events(&mut h.stoat);
        assert_eq!(
            h.stoat.index_pending_external_edits.len(),
            1,
            "the directory is clean, so its verdict caches as not ignored"
        );
        h.stoat.index_pending_external_edits.clear();

        h.fake_git().add_repo("/repo").ignored("generated");
        h.fake_fs_watcher()
            .inject(Path::new("/repo/.gitignore"), FsEventKind::Modified);
        h.fake_fs_watcher().inject(&built, FsEventKind::Modified);
        drain_fs_watch_events(&mut h.stoat);

        assert!(
            h.stoat.index_pending_external_edits.is_empty(),
            "the new rule applies rather than the verdict cached before it"
        );
    }
}
