//! Background warm pass that fills the per-file diff cache before review opens.
//!
//! Opening review otherwise pays a full working-tree scan and structural diff
//! at dispatch time. The per-file [`crate::diff_cache::DiffCache`] already lets
//! a warm open skip all diffing. The gap this closes is that nothing fills the
//! cache before the first open. [`ensure_diff_warm`] runs the whole-changeset pass
//! once per workspace on a blocking thread, writing move-aware hunks into the
//! cache, so the first `Diff` streams entirely from cache.
//!
//! A [`crate::badge::BadgeSource::DiffWarm`] badge shows while the pass runs.
//! Opening review mid-warm cancels it (see
//! [`crate::action_handlers::review::open_review`]) so the two never diff the
//! same tree twice.

use crate::{
    action_handlers::review,
    app::Stoat,
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    diff,
    diff_cache::DiffCache,
    host::{FsHost, GitHost},
    review::{extract_review_hunks_single, ReviewFileInput},
    review_session::{ReviewSession, ReviewSource},
};
use std::{
    path::{Path, PathBuf},
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
/// finishes; [`install_finished`] drains the badge on the next background pump.
/// There is no result to install, unlike [`crate::project_env`].
pub(crate) struct PendingDiffWarm {
    _task: Task<()>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl PendingDiffWarm {
    /// Signal a superseding review scan so the warm stops writing.
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// An in-flight single-file diff warm, recomputing one edited file's hunks.
///
/// Held in [`Stoat::diff_warm_files`] so the task is not dropped (which would
/// cancel it) before it writes to the cache. It flips `done` when finished, and
/// [`install_finished`] drops the completed ones and drives the shared badge.
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
    if stoat.pending_diff_warm.is_some() || stoat.pending_review_scan.is_some() {
        return;
    }
    {
        let ws = stoat.active_workspace();
        if ws.review.is_some() || ws.diff_warmed {
            return;
        }
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
    stoat.pending_diff_warm = Some(PendingDiffWarm {
        _task: task,
        cancel,
        done,
    });

    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::DiffWarm);
    ws.badges.insert(Badge {
        source: BadgeSource::DiffWarm,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: "computing diff".to_string(),
        detail: None,
    });
}

/// Spawn a single-file diff warm for `path`, recomputing its HEAD-vs-worktree
/// hunks into the cache move-unaware.
///
/// The move-unaware entry gives an instant open, and the whole-changeset
/// Complete pass on the next review open upgrades it (see the `move_aware` flag
/// on [`crate::diff_cache::DiffCache`]). Posts the DiffWarm badge, which
/// [`install_finished`] drops once every warm finishes. Called from
/// [`Stoat::drain_pending_diff_warm_files`] after the per-path debounce fires.
pub(crate) fn spawn_file_warm(stoat: &mut Stoat, path: PathBuf) {
    let git_root = stoat.active_workspace().git_root.clone();
    let git_host = stoat.git_host.clone();
    let fs_host = stoat.fs_host.clone();
    let langs = stoat.language_registry.clone();
    let cache = stoat.diff_cache.clone();
    let redraw = stoat.redraw_notify.clone();
    let done = Arc::new(AtomicBool::new(false));

    let task = {
        let done = done.clone();
        stoat.executor.spawn_blocking(move || {
            warm_file(&*git_host, &*fs_host, &langs, &git_root, &path, &cache);
            done.store(true, Ordering::Relaxed);
            redraw.notify_one();
        })
    };
    stoat
        .diff_warm_files
        .push(PendingFileWarm { _task: task, done });

    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::DiffWarm);
    ws.badges.insert(Badge {
        source: BadgeSource::DiffWarm,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: "computing diff".to_string(),
        detail: None,
    });
}

/// Clear finished warms and drop the DiffWarm badge once none remain.
///
/// Called from [`Stoat::drive_background`]. Clears the full warm when its task
/// finishes and drops every completed single-file warm, then removes the shared
/// badge only when neither a full warm nor any file warm is still in flight.
/// No completion badge is shown on success.
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

    if stoat.pending_diff_warm.is_none() && stoat.diff_warm_files.is_empty() {
        stoat
            .active_workspace_mut()
            .badges
            .remove_by_source(BadgeSource::DiffWarm);
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
    let Some((workdir, inputs)) = diff::scan_working_tree(git, fs, langs, git_root) else {
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

    let mut session = ReviewSession::new(ReviewSource::WorkingTree { workdir });
    session.add_files(missing);
    review::populate_diff_cache_from(cache, &session, cancel);
}

/// Recompute one edited file's HEAD-vs-worktree hunks and write them to the
/// cache move-unaware.
///
/// Skips a file untracked in HEAD, which has nothing to diff against, and a file
/// clean vs HEAD, which yields no hunks. Builds the same base/buffer/language
/// the review scan reads so the cache key matches and a later open hits it.
fn warm_file(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    path: &Path,
    cache: &Mutex<DiffCache>,
) {
    let Some(repo) = git.discover(git_root) else {
        return;
    };
    let Some(workdir) = repo.workdir() else {
        return;
    };
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
        .strip_prefix(&workdir)
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

    let hunks = extract_review_hunks_single(&input, 3, None);
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
    use crate::{
        action_handlers::review::diff_cache_key,
        badge::{BadgeSource, BadgeState},
        test_harness::TestHarness,
    };

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
    fn warm_badge_shows_then_clears() {
        let mut h = warm_harness();
        ensure_diff_warm(&mut h.stoat);

        let badge = h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::DiffWarm)
            .expect("badge posted at spawn");
        assert_eq!(
            h.stoat.active_workspace().badges.get(badge).unwrap().state,
            BadgeState::Active
        );

        h.settle();
        install_finished(&mut h.stoat);
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::DiffWarm)
                .is_none(),
            "badge cleared once the warm finishes"
        );
    }

    #[test]
    fn precompute_disabled_spawns_no_warm() {
        let mut h = warm_harness();
        h.stoat.settings.review_precompute = Some(false);
        ensure_diff_warm(&mut h.stoat);
        assert!(h.stoat.pending_diff_warm.is_none());
        assert!(h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::DiffWarm)
            .is_none());
    }

    /// Drive one debounced fs-watch event for `path` through to the single-file
    /// warm, mirroring the run loop's update() drains.
    fn drive_fs_event(h: &mut TestHarness, path: &Path, kind: crate::host::FsEventKind) {
        h.fake_fs_watcher().inject(path, kind);
        h.stoat.drain_fs_watch_events();
        h.advance_clock(crate::app::REVIEW_EXTERNAL_EDIT_DEBOUNCE);
        h.stoat.drain_pending_diff_warm_files();
        h.stoat.drain_pending_git_refresh();
        h.settle();
    }

    #[test]
    fn fs_watch_modified_warms_the_file() {
        let mut h = warm_harness();
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
