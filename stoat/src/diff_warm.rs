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
pub struct PendingDiffWarm {
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
pub fn ensure_diff_warm(stoat: &mut Stoat) {
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
pub fn install_finished(stoat: &mut Stoat) {
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
