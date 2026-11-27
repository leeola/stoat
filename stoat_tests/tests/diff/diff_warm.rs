use std::path::Path;
use stoat::{
    action_handlers::review::diff_cache_key,
    debounce,
    diff_warm::{ensure_diff_warm, install_finished},
    test_harness::TestHarness,
};

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
fn drive_fs_event(h: &mut TestHarness, path: &Path, kind: stoat::host::FsEventKind) {
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
        stoat::host::FsEventKind::Modified,
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
        stoat::host::FsEventKind::Modified,
    );

    assert!(
        !h.stoat.active_workspace().diff_warmed,
        "a .git event clears diff_warmed so the full warm re-runs",
    );
}
