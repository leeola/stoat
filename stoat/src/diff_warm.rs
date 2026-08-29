//! Background warm pass that fills the per-file diff cache before review opens.
//!
//! Opening review otherwise pays a full working-tree scan and structural diff
//! at dispatch time. The per-file [`crate::diff_cache::DiffCache`] already lets
//! a warm open skip all diffing. The gap this closes is that nothing fills the
//! cache before the first open. [`ensure_diff_warm`] runs the whole-changeset pass
//! once per workspace on a blocking thread, writing move-aware hunks into the
//! cache, so the first `Diff` streams entirely from cache.
//!
//! The focused pane's status bar shows a diff spinner segment while the pass
//! runs, gated by [`crate::app::Stoat::diff_warm_busy`]. Opening review
//! mid-warm cancels it (see
//! [`crate::action_handlers::review::open_review`]) so the two never diff the
//! same tree twice.

use crate::{
    action_handlers::review,
    app::Stoat,
    diff,
    diff_cache::DiffCache,
    host::{FsHost, GitHost, GitRepo},
    review::{extract_review_hunks_single, ReviewFileInput},
    review_session::DiffDocument,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat_language::{structural_diff::TreeCache, LanguageRegistry};
use stoat_scheduler::Task;

/// An in-flight background warm pass.
///
/// The task writes straight into the shared cache and flips `done` when it
/// finishes. [`install_finished`] clears it on the next background pump. There
/// is no result to install, unlike [`crate::project_env`].
pub(crate) struct PendingDiffWarm {
    _task: Task<()>,
    done: Arc<AtomicBool>,
}

/// An in-flight single-file diff warm, recomputing one edited file's hunks.
///
/// Held in [`Stoat::diff_warm_files`] so the task is not dropped (which would
/// cancel it) before it writes to the cache. It flips `done` when finished, and
/// [`install_finished`] drops the completed ones.
pub(crate) struct PendingFileWarm {
    _task: Task<()>,
    done: Arc<AtomicBool>,
}

/// Start the active workspace's background diff warm if it has not run yet.
///
/// No-op unless [`Stoat::diff_warm_auto`] is on (so the test harness never
/// warms unbidden) and `review.precompute` is enabled. Skips when a review
/// session or scan is already active or a warm is already pending, and runs at
/// most once per workspace via the `diff_warmed` flag, reset when the cwd
/// changes.
pub(crate) fn ensure_diff_warm(stoat: &mut Stoat) {
    if !stoat.diff_warm_auto || !stoat.settings.review_precompute.unwrap_or(true) {
        return;
    }
    if stoat.pending_diff_warm.is_some() {
        return;
    }
    if stoat.active_workspace().diff_warmed {
        return;
    }
    stoat.active_workspace_mut().diff_warmed = true;

    let git_root = stoat.active_workspace().git_root.clone();
    let git_host = stoat.git_host.clone();
    let fs_host = stoat.fs_host.clone();
    let langs = stoat.language_registry.clone();
    let cache = stoat.diff_cache.clone();
    let redraw = stoat.redraw_notify.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    let task = {
        let cancel = cancel.clone();
        let done = done.clone();
        stoat.executor.spawn_blocking(move || {
            warm(&*git_host, &*fs_host, &langs, &git_root, &cache, &cancel);
            done.store(true, Ordering::Relaxed);
            redraw.notify_one();
        })
    };
    stoat.pending_diff_warm = Some(PendingDiffWarm { _task: task, done });
}

/// Spawn one diff warm covering `paths`, recomputing their HEAD-vs-worktree
/// hunks into the cache move-unaware.
///
/// The move-unaware entry gives an instant open, and the whole-changeset
/// Complete pass on the next review open upgrades it (see the `move_aware` flag
/// on [`crate::diff_cache::DiffCache`]). The status bar shows a diff spinner
/// segment until [`install_finished`] clears every warm. Called from
/// [`crate::debounce::drain_pending_diff_warm_files`] once the shared debounce
/// window closes.
///
/// One job for the batch rather than one per path, so a burst opens the
/// repository once instead of once per file.
pub(crate) fn spawn_file_warm(stoat: &mut Stoat, paths: Vec<PathBuf>) {
    let git_root = stoat.active_workspace().git_root.clone();
    let git_host = stoat.git_host.clone();
    let fs_host = stoat.fs_host.clone();
    let langs = stoat.language_registry.clone();
    let cache = stoat.diff_cache.clone();
    let tree_cache = stoat.diff_tree_cache.clone();
    let redraw = stoat.redraw_notify.clone();
    let done = Arc::new(AtomicBool::new(false));

    let task = {
        let done = done.clone();
        stoat.executor.spawn_blocking(move || {
            warm_files(
                &*git_host,
                &*fs_host,
                &langs,
                &git_root,
                &paths,
                &cache,
                &tree_cache,
            );
            done.store(true, Ordering::Relaxed);
            redraw.notify_one();
        })
    };
    stoat
        .diff_warm_files
        .push(PendingFileWarm { _task: task, done });
}

/// Clear finished warms, so [`Stoat::diff_warm_busy`] falls once none remain.
///
/// Called from [`Stoat::drive_background`]. Clears the full warm when its task
/// finishes and drops every completed single-file warm. The status bar's diff
/// segment then hides on the next frame once neither a full warm nor any file
/// warm is still in flight.
pub(crate) fn install_finished(stoat: &mut Stoat) {
    if stoat
        .pending_diff_warm
        .as_ref()
        .is_some_and(|w| w.done.load(Ordering::Relaxed))
    {
        stoat.pending_diff_warm = None;
    }
    stoat
        .diff_warm_files
        .retain(|w| !w.done.load(Ordering::Relaxed));
}

/// Scan the worktree, skip files already cached, and write the misses'
/// move-aware hunks into the cache.
///
/// Runs the whole-changeset pass over the missing files so cross-file moves are
/// captured, then writes each file move-aware. `cancel` is honored before the
/// diff and between cache writes so a superseding scan stops it promptly.
fn warm(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    cache: &Mutex<DiffCache>,
    cancel: &AtomicBool,
) {
    let Some((_workdir, inputs)) = diff::scan_working_tree(git, fs, langs, git_root) else {
        return;
    };
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    let missing: Vec<_> = inputs
        .into_iter()
        .filter(|input| {
            let key = review::diff_cache_key(
                &input.base_text,
                &input.buffer_text,
                input.language.as_ref(),
            );
            cache
                .lock()
                .expect("diff_cache poisoned")
                .lookup(&key)
                .is_none()
        })
        .collect();
    if missing.is_empty() || cancel.load(Ordering::Relaxed) {
        return;
    }

    let mut doc = DiffDocument::default();
    doc.add_files(missing);
    review::populate_diff_cache_from(cache, &doc, cancel);
}

/// Recompute the edited files' HEAD-vs-worktree hunks and write them to the
/// cache move-unaware.
///
/// Opens the repository once for the whole batch, which is what a burst of
/// external edits costs on the blocking pool.
fn warm_files(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    paths: &[PathBuf],
    cache: &Mutex<DiffCache>,
    tree_cache: &TreeCache,
) {
    let Some(repo) = git.discover(git_root) else {
        return;
    };
    let Some(workdir) = repo.workdir() else {
        return;
    };

    for path in paths {
        warm_file_in(&*repo, fs, langs, &workdir, path, cache, tree_cache);
    }
}

/// Warm one path against an already-open repository.
///
/// Takes the repository and its workdir rather than a git root, so a batch pays
/// the libgit2 open once instead of once per file.
///
/// Skips a file untracked in HEAD, which has nothing to diff against, and a
/// file clean vs HEAD, which yields no hunks. Builds the same
/// base/buffer/language the review scan reads so the cache key matches and a
/// later open hits it.
fn warm_file_in(
    repo: &dyn GitRepo,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    workdir: &Path,
    path: &Path,
    cache: &Mutex<DiffCache>,
    tree_cache: &TreeCache,
) {
    let Some(base_text) = repo.head_content(path) else {
        return;
    };
    let buffer_text = match diff::read_utf8(fs, path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return,
    };

    let language = langs.for_path(path);
    let rel_path = path
        .strip_prefix(workdir)
        .unwrap_or(path)
        .display()
        .to_string();
    let input = ReviewFileInput {
        path: path.to_path_buf(),
        rel_path,
        language: language.clone(),
        base_text: Arc::new(base_text),
        buffer_text: Arc::new(buffer_text),
    };

    let hunks = extract_review_hunks_single(&input, 3, None, Some(tree_cache));
    if hunks.is_empty() {
        return;
    }
    let key = review::diff_cache_key(
        &input.base_text,
        &input.buffer_text,
        input.language.as_ref(),
    );
    cache
        .lock()
        .expect("diff_cache poisoned")
        .insert(key, Arc::new(hunks), false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action_handlers::review::diff_cache_key, debounce, test_harness::TestHarness};

    /// A harness with one changed file and diff-warming enabled.
    fn warm_harness() -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);
        h
    }

    #[test]
    fn warm_populates_cache_move_aware() {
        let mut h = warm_harness();
        ensure_diff_warm(&mut h.stoat);
        h.settle();

        let key = diff_cache_key("a\n", "b\n", None);
        let cache = h.stoat.diff_cache();
        let (_, move_aware) = cache
            .lock()
            .expect("diff_cache")
            .lookup(&key)
            .expect("warm populated the cache");
        assert!(move_aware, "warm writes move-aware hunks");
    }

    #[test]
    fn warm_marks_busy_then_clears() {
        let mut h = warm_harness();
        ensure_diff_warm(&mut h.stoat);
        assert!(h.stoat.diff_warm_busy(), "busy while the warm is pending");

        h.settle();
        install_finished(&mut h.stoat);
        assert!(
            !h.stoat.diff_warm_busy(),
            "no longer busy once the warm finishes"
        );
    }

    #[test]
    fn precompute_disabled_spawns_no_warm() {
        let mut h = warm_harness();
        h.stoat.settings.review_precompute = Some(false);
        ensure_diff_warm(&mut h.stoat);
        assert!(h.stoat.pending_diff_warm.is_none());
        assert!(!h.stoat.diff_warm_busy());
    }

    /// Drive one debounced fs-watch event for `path` through to the single-file
    /// warm, mirroring the run loop's update() drains.
    fn drive_fs_event(h: &mut TestHarness, path: &Path, kind: crate::host::FsEventKind) {
        h.fake_fs_watcher().inject(path, kind);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(debounce::FS_WATCH_DEBOUNCE);
        debounce::drain_pending_diff_warm_files(&mut h.stoat);
        debounce::drain_pending_diff_refresh(&mut h.stoat);
        h.settle();
    }

    #[test]
    fn fs_watch_modified_warms_the_file() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;
        drive_fs_event(
            &mut h,
            Path::new("/repo/a.txt"),
            crate::host::FsEventKind::Modified,
        );

        let key = diff_cache_key("a\n", "b\n", None);
        let (_, move_aware) = h
            .stoat
            .diff_cache()
            .lock()
            .expect("diff_cache")
            .lookup(&key)
            .expect("the fs-watch warm cached the edited file");
        assert!(!move_aware, "an incremental warm writes move-unaware hunks");
    }

    #[test]
    fn fs_watch_gitignored_path_caches_nothing() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;
        h.fake_git().add_repo("/repo").ignored("a.txt");
        drive_fs_event(
            &mut h,
            Path::new("/repo/a.txt"),
            crate::host::FsEventKind::Modified,
        );

        let key = diff_cache_key("a\n", "b\n", None);
        assert!(
            h.stoat
                .diff_cache()
                .lock()
                .expect("diff_cache")
                .lookup(&key)
                .is_none(),
            "a gitignored path is never warmed",
        );
    }

    /// A checkout's worth of paths is not warmed a file at a time.
    ///
    /// Each per-file warm opens the repository and reads two versions of the
    /// file. A large batch therefore costs more than the whole-changeset pass it
    /// duplicates. Clearing `diff_warmed` hands the batch to that pass.
    #[test]
    fn a_large_batch_defers_to_the_whole_warm() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;

        for i in 0..200 {
            let path = PathBuf::from(format!("/repo/f{i}.txt"));
            h.fake_fs().insert_file(&path, "x\n");
            h.fake_fs_watcher()
                .inject(&path, crate::host::FsEventKind::Modified);
        }
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(debounce::FS_WATCH_DEBOUNCE);

        assert!(
            h.stoat.diff_warm_files.is_empty(),
            "no per-file warm spawns for a batch this size",
        );
        assert!(
            !h.stoat.active_workspace().diff_warmed,
            "and the whole warm is owed instead",
        );
    }

    /// The window runs from the first path of a burst, not the latest.
    ///
    /// A reset-on-each-event timer never fires while a checkout or a build
    /// emits paths faster than the window, which is the case the warm most
    /// needs to survive.
    #[test]
    fn the_window_runs_from_the_first_path_of_a_burst() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;

        // Two steps that clear one fixed window but never a window the second
        // path restarts.
        let step = debounce::FS_WATCH_DEBOUNCE.mul_f32(0.75);

        h.fake_fs_watcher()
            .inject(Path::new("/repo/a.txt"), crate::host::FsEventKind::Modified);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(step);

        let second = PathBuf::from("/repo/b.txt");
        h.fake_fs().insert_file(&second, "x\n");
        h.fake_fs_watcher()
            .inject(&second, crate::host::FsEventKind::Modified);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(step);

        assert!(
            !h.stoat.diff_warm_files.is_empty(),
            "the window opened by the first path closes on time",
        );
    }

    /// A burst that writes `.git` alongside working-tree files warms once.
    ///
    /// The `.git` write stales every base on its own. A per-file warm first
    /// diffs those files, and the whole warm then diffs them again.
    #[test]
    fn a_git_write_in_the_window_cancels_the_per_file_warm() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;

        h.fake_fs_watcher()
            .inject(Path::new("/repo/a.txt"), crate::host::FsEventKind::Modified);
        h.fake_fs_watcher().inject(
            Path::new("/repo/.git/index"),
            crate::host::FsEventKind::Modified,
        );
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(debounce::FS_WATCH_DEBOUNCE);

        assert!(
            h.stoat.diff_warm_files.is_empty(),
            "the shared window sees the .git write and spawns no per-file warm",
        );
        assert!(
            h.stoat
                .diff_cache()
                .lock()
                .expect("diff_cache")
                .lookup(&diff_cache_key("a\n", "b\n", None))
                .is_none(),
            "so nothing is diffed twice",
        );
    }

    #[test]
    fn fs_watch_git_event_rearms_full_warm() {
        let mut h = warm_harness();
        h.stoat.active_workspace_mut().diff_warmed = true;
        drive_fs_event(
            &mut h,
            Path::new("/repo/.git/HEAD"),
            crate::host::FsEventKind::Modified,
        );

        assert!(
            !h.stoat.active_workspace().diff_warmed,
            "a .git event clears diff_warmed so the full warm re-runs",
        );
    }
}
