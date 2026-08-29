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
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Quiet window after the last filesystem-watch event before the work that
/// event triggers runs.
///
/// Paces every consumer of the watch: a `.git` write stales the open diffs, and
/// an edited file is reindexed. Mirrors
/// [`crate::lsp::sync::LSP_DID_CHANGE_DEBOUNCE`] so a formatter-on-save burst
/// (or an agent edit chain) collapses into one pass rather than three.
pub(crate) const FS_WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(50);

/// Quiet window after the last parse of a buffer before its symbols are
/// extracted again.
///
/// An extract reads the whole file and its drain rebuilds the project's
/// adjacency, so a burst of keystrokes must collapse into one rather than
/// queue an extract each. Far longer than [`FS_WATCH_DEBOUNCE`], which is sized
/// for a formatter-on-save burst. Typing pauses less often than that, and
/// nothing reads the symbol index between keystrokes.
pub(crate) const INDEX_EDIT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a session's work stays unpersisted before the active workspace is
/// written again.
///
/// A crash or a kill loses whatever happened inside the open window, so this is
/// the bound on that loss rather than a quiet period to wait out. It paces a
/// throttle, not a debounce. The window opens on the first input after a save
/// and later input does not push it back, because a reset-on-input debounce
/// never fires at all under sustained typing, which is when the loss costs
/// most.
pub(crate) const WORKSPACE_AUTOSAVE_THROTTLE: std::time::Duration =
    std::time::Duration::from_secs(5);

/// External-change paths [`drain_pending_index_edits`] hands to the
/// blocking pool in one pass.
///
/// Each costs only a spawn on the run loop, the read and the parse having moved
/// into the job. What the cap paces is the pool: a checkout naming thousands of
/// files queues them over several windows rather than in one turn.
const INDEX_EXTERNAL_DRAIN_CAP: usize = 256;

/// Edited paths above which [`drain_pending_diff_warm_files`] gives up on
/// warming them one at a time and defers to the whole-changeset warm.
///
/// Each per-file warm reads a HEAD blob and a worktree file and runs a
/// structural diff, so a batch is only worth the per-file path while it stays
/// smaller than the changeset the wholesale warm walks anyway. A save burst or
/// a formatter run stays well under this. A checkout or a branch switch does
/// not, and that is the case this routes away.
const DIFF_WARM_BATCH_MAX: usize = 64;

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
/// [`FsWatchHost`], routing each to the debounce its path calls for.
///
/// A `.git` write goes to [`arm_diff_refresh_debounce`], a working-tree file to
/// [`arm_diff_warm_file_debounce`] and [`arm_index_external_edit_debounce`].
/// None of them does the work here: each arms a timer whose drain lands on the
/// main loop later.
///
/// Cap matches [`Stoat::drain_lsp_notifications`] so a pathological burst
/// cannot starve the event loop.
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
    // Keep the diff cache warm incrementally, gated the same as the full
    // background warm.
    let precompute = stoat.diff_warm_auto && stoat.settings.review_precompute.unwrap_or(true);
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
            // moved HEAD and staled every diff base, so it invalidates through
            // the shared debounce. Its drain clears the diff_warmed flag and
            // stales every open buffer's diff map.
            arm_diff_refresh_debounce(stoat);
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

/// Schedule a debounced diff invalidation after a git-state change under
/// `<git_root>/.git`.
///
/// The debounce is single-slot rather than per-path, so re-arming drops the
/// prior task, which cancels its future at the [`Executor::timer`] await. A
/// burst of `.git` writes from one commit then fires a single invalidation once
/// the [`FS_WATCH_DEBOUNCE`] window elapses. The main loop drains it via
/// [`drain_pending_diff_refresh`], since async tasks cannot mutate `Stoat`.
fn arm_diff_refresh_debounce(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let tx = stoat.diff_refresh_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(FS_WATCH_DEBOUNCE).await;
        let _ = tx.send(()).await;
    });
    stoat.pending_diff_refresh = Some(task);
}

/// Open the autosave window if it is closed, so the active workspace is
/// persisted [`WORKSPACE_AUTOSAVE_THROTTLE`] from now.
///
/// Returns without touching an already-armed slot, which is what makes this a
/// throttle rather than a debounce. Re-arming pushes the write back for as long
/// as the user keeps typing, and a save that never lands under sustained work is
/// the case this exists to cover.
///
/// The main loop does the save via [`drain_pending_workspace_autosave`], since
/// async tasks cannot mutate `Stoat`.
pub(crate) fn arm_workspace_autosave(stoat: &mut Stoat) {
    if stoat.pending_workspace_autosave.is_some() {
        return;
    }

    let executor = stoat.executor.clone();
    let tx = stoat.workspace_autosave_tx.clone();
    // The drain wake rather than the redraw one, since the expiry has a file to
    // write and a grid that did not move.
    let drain = stoat.drain_notify.clone();
    let task = stoat.executor.spawn_with_redraw(drain, async move {
        executor.timer(WORKSPACE_AUTOSAVE_THROTTLE).await;
        let _ = tx.send(()).await;
    });
    stoat.pending_workspace_autosave = Some(task);
}

/// Collect `path` for a debounced diff warm after an external change.
///
/// A burst shares one window rather than getting a timer each. The window opens
/// on the first path and covers every path that arrives before it closes, so a
/// checkout costs one task however many files it touches.
fn arm_diff_warm_file_debounce(stoat: &mut Stoat, path: PathBuf) {
    let opening = stoat.diff_warm_pending_paths.is_empty();
    stoat.diff_warm_pending_paths.insert(path);
    if opening {
        arm_diff_warm_file_timer(stoat);
    }
}

/// Start the shared debounce window, replacing any timer already held.
fn arm_diff_warm_file_timer(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let tx = stoat.diff_warm_file_tx.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.diff_warm_file_timer = Some(stoat.executor.spawn_with_redraw(redraw, async move {
        executor.timer(FS_WATCH_DEBOUNCE).await;
        let _ = tx.send(()).await;
    }));
}

/// Drain the diff-refresh debounce marker, staling every diff the moved HEAD
/// left describing a base that no longer exists.
///
/// Diff maps are keyed by buffer version alone, so nothing else notices that a
/// commit, reset, or branch switch moved the base under them. Gutter marks and
/// the per-file `:diff` view then follow each rebase step instead of freezing
/// at the pre-rebase base.
///
/// The background warm is re-armed with them, since every cached base moved.
///
/// Returns `false` always: the work here stales state for the next render
/// rather than producing anything a settle loop waits on.
pub(crate) fn drain_pending_diff_refresh(stoat: &mut Stoat) -> bool {
    for _ in 0..256 {
        let Ok(()) = stoat.diff_refresh_rx.try_recv() else {
            break;
        };
        stoat.pending_diff_refresh = None;
        stoat.active_workspace_mut().invalidate_all_diffs();
        stoat.active_workspace_mut().diff_warmed = false;
    }
    false
}

/// Drain the autosave throttle marker, persisting the active workspace and
/// closing the window so the next input opens a fresh one.
///
/// Only the active workspace, because a background one was already saved when
/// the user switched away from it, and quit saves them all. Routing through
/// [`Stoat::save_workspace`] is what keeps the write async and keeps the
/// fresh-session and persistence-disabled gates in one place.
///
/// Returns `false` always: the write lands on the blocking pool and produces
/// nothing a settle loop waits on.
pub(crate) fn drain_pending_workspace_autosave(stoat: &mut Stoat) -> bool {
    while stoat.workspace_autosave_rx.try_recv().is_ok() {
        stoat.pending_workspace_autosave = None;
        stoat.save_workspace(stoat.active_workspace);
    }
    false
}

/// Warm the diffs of the edited paths whose debounce window has closed.
///
/// Returns `true` if a warm spawned, so the test harness settle loop
/// re-iterates.
///
/// Two kinds of batch go to the whole-changeset warm instead. One is a batch
/// past [`DIFF_WARM_BATCH_MAX`]. The other is a batch that arrives while the
/// whole warm is already owed, which a `.git` write in the same window causes
/// and which also holds before the first warm of a workspace runs.
///
/// Both drop the set and clear `diff_warmed`, which leaves
/// [`crate::diff_warm::ensure_diff_warm`] to cover the batch in one pass that
/// skips whatever the cache already holds. That is what keeps a checkout from
/// diffing the tree file by file and then again wholesale.
pub(crate) fn drain_pending_diff_warm_files(stoat: &mut Stoat) -> bool {
    if stoat.diff_warm_file_rx.try_recv().is_err() {
        return false;
    }
    stoat.diff_warm_file_timer = None;

    let paths = std::mem::take(&mut stoat.diff_warm_pending_paths);
    if !stoat.diff_warm_auto || !stoat.settings.review_precompute.unwrap_or(true) {
        return false;
    }

    let whole_warm_is_owed =
        stoat.pending_diff_refresh.is_some() || !stoat.active_workspace().diff_warmed;
    if whole_warm_is_owed || paths.len() > DIFF_WARM_BATCH_MAX {
        stoat.active_workspace_mut().diff_warmed = false;
        return false;
    }

    crate::diff_warm::spawn_file_warm(stoat, paths.into_iter().collect());
    true
}

/// Collect `path` for a debounced reindex after an external change.
///
/// A burst shares one window rather than getting a timer each. The window opens
/// on the first path and covers every path that arrives before it closes, so a
/// checkout costs one task however many files it touches.
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
        executor.timer(FS_WATCH_DEBOUNCE).await;
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
    use futures::FutureExt;
    use std::time::Duration;
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

    /// A session that never switches workspaces persisted nothing until a
    /// clean quit, so a crash lost the lot. Input is what schedules the write.
    #[test]
    fn a_key_press_arms_the_autosave_throttle() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        assert!(
            h.stoat.pending_workspace_autosave.is_none(),
            "a session with no input owes no save"
        );

        h.type_keys("i");

        assert!(
            h.stoat.pending_workspace_autosave.is_some(),
            "the first press opens the window"
        );
    }

    /// The distinction that matters under sustained typing. A debounce restarts
    /// its window on every press and so never fires while the user works, which
    /// is exactly when losing the session costs most.
    #[test]
    fn typing_through_the_window_still_saves_on_time() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.type_keys("i");

        h.advance_clock(Duration::from_secs(3));
        h.type_keys("x");
        h.advance_clock(Duration::from_secs(3));

        assert!(
            h.stoat.pending_workspace_autosave.is_none(),
            "the window opened at the first press closed at 5s and the save ran, \
             where a debounce restarted at 3s and would still be waiting"
        );
    }

    /// The expiry has a file to write and a grid that did not move. Waking the
    /// loop to paint re-emits every decoration stream for nothing.
    ///
    /// Armed directly rather than by typing, because a keystroke also arms the
    /// index and diff debounces, and those do ask for a frame.
    #[test]
    fn the_autosave_expiry_wakes_the_loop_to_drain_and_not_to_paint() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        arm_workspace_autosave(&mut h.stoat);
        assert!(h.stoat.pending_workspace_autosave.is_some());

        h.advance_clock(WORKSPACE_AUTOSAVE_THROTTLE + Duration::from_secs(1));

        assert!(
            h.stoat.drain_notify.notified().now_or_never().is_some(),
            "the expiry woke the loop to drain",
        );
        assert!(
            h.stoat.redraw_notify.notified().now_or_never().is_none(),
            "and asked for no frame of its own",
        );
    }

    /// The window has to close as well as open, or the second burst of work
    /// never schedules a write of its own.
    #[test]
    fn the_drain_clears_the_armed_slot() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.type_keys("i");
        h.stoat
            .workspace_autosave_tx
            .try_send(())
            .expect("stand in for the timer firing");

        let progress = drain_pending_workspace_autosave(&mut h.stoat);

        assert_eq!(
            (progress, h.stoat.pending_workspace_autosave.is_some()),
            (false, false),
            "the drain closes the window and reports no work a settle waits on",
        );
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
            h.stoat.diff_warm_pending_paths.is_empty(),
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
