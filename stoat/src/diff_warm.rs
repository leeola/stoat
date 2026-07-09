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
    review_session::{ReviewSession, ReviewSource},
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

    #[cfg(test)]
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
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

/// Drop the DiffWarm badge and clear the pending warm once the task finishes.
///
/// Called from [`Stoat::drive_background`]. A no-op while the warm still runs
/// or when none is pending. Silent on success: no completion badge.
pub(crate) fn install_finished(stoat: &mut Stoat) {
    let done = stoat
        .pending_diff_warm
        .as_ref()
        .is_some_and(|w| w.done.load(Ordering::Relaxed));
    if !done {
        return;
    }
    stoat.pending_diff_warm = None;
    stoat
        .active_workspace_mut()
        .badges
        .remove_by_source(BadgeSource::DiffWarm);
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

    #[test]
    fn open_review_cancels_pending_warm() {
        let mut h = warm_harness();
        ensure_diff_warm(&mut h.stoat);
        assert!(h.stoat.pending_diff_warm.is_some());

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff);
        assert!(
            h.stoat
                .pending_diff_warm
                .as_ref()
                .expect("warm still pending")
                .cancelled(),
            "opening review cancels the in-flight warm"
        );
    }
}
