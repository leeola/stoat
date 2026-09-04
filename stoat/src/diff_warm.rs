//! Background pass that fills [`crate::diff_cache::DiffCache`] for the viewport
//! diff RPC.
//!
//! The cache serves that one reader. The gutter builds its own diff map and the
//! review screen scans the working tree itself, so neither reads a thing this
//! writes. A session with no client attached would warm hunks nobody looks at,
//! which is why the pass is opt-in through `review.precompute` and off by
//! default.
//!
//! Where it is on, [`ensure_diff_warm`] runs the whole-changeset pass once per
//! workspace on a blocking thread and writes move-aware hunks. The focused
//! pane's status bar shows a diff spinner while it runs, gated by
//! [`crate::app::Stoat::diff_warm_busy`].

use crate::{
    action_handlers::review,
    app::Stoat,
    diff,
    diff_cache::DiffCache,
    host::{FsHost, GitHost},
    review_session::DiffDocument,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat_language::LanguageRegistry;
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

/// Start the active workspace's background diff warm if it has not run yet.
///
/// No-op unless [`Stoat::diff_warm_auto`] is on (so the test harness never
/// warms unbidden) and `review.precompute` is enabled. Skips when a review
/// session or scan is already active or a warm is already pending, and runs at
/// most once per workspace via the `diff_warmed` flag, reset when the cwd
/// changes.
pub(crate) fn ensure_diff_warm(stoat: &mut Stoat) {
    if !stoat.diff_warm_auto || !stoat.settings.review_precompute.unwrap_or(false) {
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

/// Clear the warm once it finishes, so [`Stoat::diff_warm_busy`] falls with it.
///
/// Called from [`Stoat::drive_background`]. The status bar's diff segment hides
/// on the next frame once nothing is in flight.
pub(crate) fn install_finished(stoat: &mut Stoat) {
    if stoat
        .pending_diff_warm
        .as_ref()
        .is_some_and(|w| w.done.load(Ordering::Relaxed))
    {
        stoat.pending_diff_warm = None;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action_handlers::review::diff_cache_key, debounce, test_harness::TestHarness};

    /// A harness with one changed file, diff-warming enabled, and the opt-in
    /// setting on, since the warm is off by default.
    fn warm_harness() -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.stoat.settings.review_precompute = Some(true);
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

    /// Drive one debounced fs-watch event for `path` through to its drain,
    /// mirroring the run loop's update() passes.
    fn drive_fs_event(h: &mut TestHarness, path: &Path, kind: crate::host::FsEventKind) {
        h.fake_fs_watcher().inject(path, kind);
        debounce::drain_fs_watch_events(&mut h.stoat);
        h.advance_clock(debounce::FS_WATCH_DEBOUNCE);
        debounce::drain_pending_diff_refresh(&mut h.stoat);
        h.settle();
    }

    /// The cache the warm fills has one reader, the viewport diff RPC, and a
    /// session with no client attached has nobody to read it. An external edit
    /// used to spawn a structural diff per file into it regardless.
    #[test]
    fn an_external_edit_spawns_no_diff_warm_by_default() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);

        drive_fs_event(
            &mut h,
            Path::new("/repo/a.txt"),
            crate::host::FsEventKind::Modified,
        );

        assert_eq!(
            (
                h.stoat.diff_warm_busy(),
                h.stoat
                    .diff_cache()
                    .lock()
                    .expect("diff_cache")
                    .lookup(&diff_cache_key("a\n", "b\n", None))
                    .is_some(),
            ),
            (false, false),
            "the edit warmed nothing, because nothing would read it",
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
