//! Real-filesystem round-trip tests for [`stoat::host::LocalFsWatcher`].
//!
//! Carve-out from the project's "real-fs in tests is a code smell" rule:
//! a watcher backend cannot be verified without real filesystem events,
//! the same trade-off [`local_fs`] and [`local_git`] already take.

use std::{
    thread::sleep,
    time::{Duration, Instant},
};
use stoat::host::{FsEventKind, FsWatchHost, LocalFsWatcher};
use tempfile::tempdir;

/// Maximum time to wait for a notify event before failing. macOS
/// FSEvents can take >100 ms to deliver the first event after a watch
/// is registered; pad generously to keep the test stable on busy CI.
const POLL_BUDGET: Duration = Duration::from_secs(2);

/// Polling interval. Short enough that a fast event lands in one or two
/// ticks; long enough that the test isn't a tight spin loop.
const POLL_TICK: Duration = Duration::from_millis(20);

fn drain_events_until(
    watcher: &LocalFsWatcher,
    deadline: Instant,
    pred: impl Fn(&[stoat::host::FsWatchEvent]) -> bool,
) -> Vec<stoat::host::FsWatchEvent> {
    let mut events = Vec::new();
    while Instant::now() < deadline {
        while let Some(ev) = watcher.try_recv() {
            events.push(ev);
        }
        if pred(&events) {
            return events;
        }
        sleep(POLL_TICK);
    }
    events
}

#[test]
fn watch_emits_modified_on_write() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"initial").unwrap();

    let watcher = LocalFsWatcher::new().unwrap();
    let _token = watcher.watch(&path).unwrap();

    // Some backends (FSEvents) need a beat to register the watch before
    // mutations are observed; without this the write may slip in before
    // the watcher is armed.
    sleep(Duration::from_millis(50));
    std::fs::write(&path, b"changed").unwrap();

    let deadline = Instant::now() + POLL_BUDGET;
    let events = drain_events_until(&watcher, deadline, |evs| {
        evs.iter()
            .any(|e| e.path.ends_with("file.txt") && e.kind == FsEventKind::Modified)
    });
    assert!(
        events
            .iter()
            .any(|e| e.path.ends_with("file.txt") && e.kind == FsEventKind::Modified),
        "expected a Modified event for file.txt, got {events:?}",
    );
}

#[test]
fn unwatch_stops_event_delivery() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"v1").unwrap();

    let watcher = LocalFsWatcher::new().unwrap();
    let token = watcher.watch(&path).unwrap();
    sleep(Duration::from_millis(50));
    watcher.unwatch(token);
    // Drain any events that landed before unwatch took effect.
    while watcher.try_recv().is_some() {}

    sleep(Duration::from_millis(50));
    std::fs::write(&path, b"v2").unwrap();

    sleep(Duration::from_millis(200));
    let post_unwatch: Vec<_> = std::iter::from_fn(|| watcher.try_recv()).collect();
    assert!(
        post_unwatch.is_empty(),
        "no events expected after unwatch, got {post_unwatch:?}",
    );
}
