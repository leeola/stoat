use crate::{buffer::Buffer, globals::FsWatchHostGlobal};
use gpui::{Context, EventEmitter, Task, WeakEntity};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

const TICK_PERIOD: Duration = Duration::from_millis(50);
const MAX_EVENTS_PER_TICK: usize = 256;

/// Per-workspace driver that drains the app-global
/// [`crate::globals::FsWatchHostGlobal`] on a 50ms foreground tick
/// and forwards each event to the matching tracked
/// [`Entity<Buffer>`].
///
/// Workspaces register opened buffers via [`Self::track`] and drop
/// them via [`Self::untrack`]; the driver looks up the path on
/// each drained event, calls
/// [`Buffer::reload`] on the matching entity, and emits
/// [`FsWatcherDriverEvent::ExternalEdit`] so the diff coordinator
/// (and future review pipeline) can refresh without re-reading the
/// host themselves.
///
/// Beyond tracked buffers, workspaces register *live roots* via
/// [`Self::add_live_root`]: directories whose untracked descendants
/// should still emit [`FsWatcherDriverEvent::ExternalEdit`], with no
/// buffer reload since nothing is open for them. This backs review
/// live mode, where edits to unopened files under the reviewed
/// workdir must surface.
///
/// The tick re-arms itself after every fire so it persists for the
/// driver's lifetime; when the entity drops, the next
/// `WeakEntity::update` short-circuits and the chain terminates.
pub struct FsWatcherDriver {
    tracked: HashMap<PathBuf, WeakEntity<Buffer>>,
    live_roots: Vec<PathBuf>,
    _tick_task: Option<Task<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsWatcherDriverEvent {
    ExternalEdit { path: PathBuf },
}

impl EventEmitter<FsWatcherDriverEvent> for FsWatcherDriver {}

impl FsWatcherDriver {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let mut driver = Self {
            tracked: HashMap::new(),
            live_roots: Vec::new(),
            _tick_task: None,
        };
        driver.schedule_tick(cx);
        driver
    }

    pub fn track(&mut self, path: PathBuf, buffer: gpui::Entity<Buffer>) {
        self.tracked.insert(path, buffer.downgrade());
    }

    pub fn untrack(&mut self, path: &Path) {
        self.tracked.remove(path);
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// Register `root` as a live directory: untracked event paths
    /// beneath it emit [`FsWatcherDriverEvent::ExternalEdit`]. Idempotent.
    pub fn add_live_root(&mut self, root: PathBuf) {
        if !self.live_roots.contains(&root) {
            self.live_roots.push(root);
        }
    }

    pub fn remove_live_root(&mut self, root: &Path) {
        self.live_roots.retain(|r| r != root);
    }

    pub fn live_root_count(&self) -> usize {
        self.live_roots.len()
    }

    /// Look up the live [`Entity<Buffer>`] previously registered for
    /// `path`. Returns `None` when no buffer is tracked for that
    /// path or when the prior entity has dropped. Callers use this
    /// to apply path-keyed mutations (multi-file LSP workspace
    /// edits, rename refactors) without owning their own path-to-
    /// entity map.
    pub fn buffer_for_path(&self, path: &Path) -> Option<gpui::Entity<Buffer>> {
        self.tracked.get(path)?.upgrade()
    }

    fn schedule_tick(&mut self, cx: &mut Context<'_, Self>) {
        let Some(executor) = cx
            .try_global::<crate::globals::ExecutorGlobal>()
            .map(|g| g.0.clone())
        else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            executor.timer(TICK_PERIOD).await;
            let _ = this.update(cx, |this, cx| {
                this.tick(cx);
                this.schedule_tick(cx);
            });
        });
        self._tick_task = Some(task);
    }

    fn tick(&mut self, cx: &mut Context<'_, Self>) {
        let Some(host) = cx.try_global::<FsWatchHostGlobal>().map(|g| g.0.clone()) else {
            return;
        };
        for _ in 0..MAX_EVENTS_PER_TICK {
            let Some(event) = host.try_recv() else {
                break;
            };
            let Some(weak) = self.tracked.get(&event.path).cloned() else {
                if self.under_live_root(&event.path) {
                    cx.emit(FsWatcherDriverEvent::ExternalEdit {
                        path: event.path.clone(),
                    });
                }
                continue;
            };
            match weak.upgrade() {
                Some(buffer) => {
                    buffer.update(cx, |b, cx| b.reload(cx));
                    cx.emit(FsWatcherDriverEvent::ExternalEdit {
                        path: event.path.clone(),
                    });
                },
                _ => {
                    self.tracked.remove(&event.path);
                },
            }
        }
    }

    fn under_live_root(&self, path: &Path) -> bool {
        self.live_roots.iter().any(|root| path.starts_with(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::BufferEvent,
        globals::{ExecutorGlobal, FsWatchHostGlobal},
    };
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::{buffer::BufferId, host::FsWatchHost};
    use stoat_host::{FakeFsWatcher, FsEventKind};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> (Arc<FakeFsWatcher>, Arc<TestScheduler>) {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = Executor::new(scheduler.clone());
        let fs_watcher = Arc::new(FakeFsWatcher::new());
        let fs_watcher_for_global = fs_watcher.clone();
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(FsWatchHostGlobal(
                fs_watcher_for_global as Arc<dyn FsWatchHost>,
            ));
        });
        (fs_watcher, scheduler)
    }

    fn new_driver(cx: &mut TestAppContext) -> Entity<FsWatcherDriver> {
        cx.update(|cx| cx.new(FsWatcherDriver::new))
    }

    fn new_buffer(cx: &mut TestAppContext) -> Entity<Buffer> {
        cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), "")))
    }

    fn advance(scheduler: &Arc<TestScheduler>, cx: &mut TestAppContext) {
        cx.executor().advance_clock(TICK_PERIOD);
        scheduler.advance_clock(TICK_PERIOD);
        cx.run_until_parked();
    }

    struct BufferEventRecorder {
        _subscription: Subscription,
    }

    impl BufferEventRecorder {
        fn install(
            cx: &mut TestAppContext,
            buffer: &Entity<Buffer>,
        ) -> (Entity<BufferEventRecorder>, Arc<Mutex<Vec<BufferEvent>>>) {
            let events: Arc<Mutex<Vec<BufferEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let buffer = buffer.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&buffer, move |_, _, event: &BufferEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    BufferEventRecorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    struct DriverEventRecorder {
        _subscription: Subscription,
    }

    impl DriverEventRecorder {
        fn install(
            cx: &mut TestAppContext,
            driver: &Entity<FsWatcherDriver>,
        ) -> (
            Entity<DriverEventRecorder>,
            Arc<Mutex<Vec<FsWatcherDriverEvent>>>,
        ) {
            let events: Arc<Mutex<Vec<FsWatcherDriverEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let driver = driver.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&driver, move |_, _, event: &FsWatcherDriverEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    DriverEventRecorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    #[test]
    fn new_starts_with_no_tracked_paths() {
        let mut cx = TestAppContext::single();
        let _ = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        driver.read_with(&cx, |d, _| assert_eq!(d.tracked_count(), 0));
    }

    #[test]
    fn track_and_untrack_round_trip() {
        let mut cx = TestAppContext::single();
        let _ = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let buffer = new_buffer(&mut cx);
        let path = PathBuf::from("/repo/a.rs");

        driver.update(&mut cx, |d, _| d.track(path.clone(), buffer));
        driver.read_with(&cx, |d, _| assert_eq!(d.tracked_count(), 1));

        driver.update(&mut cx, |d, _| d.untrack(&path));
        driver.read_with(&cx, |d, _| assert_eq!(d.tracked_count(), 0));
    }

    #[test]
    fn tick_with_fake_event_reloads_tracked_buffer() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let buffer = new_buffer(&mut cx);
        let path = PathBuf::from("/repo/a.rs");

        driver.update(&mut cx, |d, _| d.track(path.clone(), buffer.clone()));
        let (_recorder, events) = BufferEventRecorder::install(&mut cx, &buffer);

        fs_watcher.inject(&path, FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        let observed = events.lock().expect("recorder mutex").clone();
        assert_eq!(observed, vec![BufferEvent::Reloaded]);
    }

    #[test]
    fn tick_emits_external_edit_event() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let buffer = new_buffer(&mut cx);
        let path = PathBuf::from("/repo/a.rs");

        driver.update(&mut cx, |d, _| d.track(path.clone(), buffer.clone()));
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        fs_watcher.inject(&path, FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        let observed = events.lock().expect("recorder mutex").clone();
        assert_eq!(observed, vec![FsWatcherDriverEvent::ExternalEdit { path }]);
        drop(buffer);
    }

    #[test]
    fn untracked_path_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        fs_watcher.inject("/repo/unknown.rs", FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        assert!(events.lock().expect("recorder mutex").is_empty());
    }

    #[test]
    fn dropped_buffer_is_pruned_from_tracking() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let path = PathBuf::from("/repo/dropped.rs");
        {
            let buffer = new_buffer(&mut cx);
            driver.update(&mut cx, |d, _| d.track(path.clone(), buffer));
        }
        cx.run_until_parked();

        fs_watcher.inject(&path, FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        driver.read_with(&cx, |d, _| assert_eq!(d.tracked_count(), 0));
    }

    #[test]
    fn add_live_root_dedups_and_remove_clears() {
        let mut cx = TestAppContext::single();
        let _ = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let root = PathBuf::from("/repo");

        driver.update(&mut cx, |d, _| {
            d.add_live_root(root.clone());
            d.add_live_root(root.clone());
        });
        driver.read_with(&cx, |d, _| assert_eq!(d.live_root_count(), 1));

        driver.update(&mut cx, |d, _| d.remove_live_root(&root));
        driver.read_with(&cx, |d, _| assert_eq!(d.live_root_count(), 0));
    }

    #[test]
    fn untracked_path_under_live_root_emits_without_reload() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        driver.update(&mut cx, |d, _| d.add_live_root(PathBuf::from("/repo")));
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        let path = PathBuf::from("/repo/sub/new.rs");
        fs_watcher.inject(&path, FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        let observed = events.lock().expect("recorder mutex").clone();
        assert_eq!(observed, vec![FsWatcherDriverEvent::ExternalEdit { path }]);
    }

    #[test]
    fn untracked_path_outside_live_root_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        driver.update(&mut cx, |d, _| d.add_live_root(PathBuf::from("/repo")));
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        fs_watcher.inject("/other/x.rs", FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        assert!(events.lock().expect("recorder mutex").is_empty());
    }

    #[test]
    fn removed_live_root_stops_emitting() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let root = PathBuf::from("/repo");
        driver.update(&mut cx, |d, _| {
            d.add_live_root(root.clone());
            d.remove_live_root(&root);
        });
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        fs_watcher.inject("/repo/sub/new.rs", FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        assert!(events.lock().expect("recorder mutex").is_empty());
    }

    #[test]
    fn tracked_buffer_under_live_root_emits_once() {
        let mut cx = TestAppContext::single();
        let (fs_watcher, scheduler) = install_globals(&mut cx);
        let driver = new_driver(&mut cx);
        let buffer = new_buffer(&mut cx);
        let path = PathBuf::from("/repo/a.rs");

        driver.update(&mut cx, |d, _| {
            d.track(path.clone(), buffer.clone());
            d.add_live_root(PathBuf::from("/repo"));
        });
        let (_recorder, events) = DriverEventRecorder::install(&mut cx, &driver);

        fs_watcher.inject(&path, FsEventKind::Modified);
        advance(&scheduler, &mut cx);

        let observed = events.lock().expect("recorder mutex").clone();
        assert_eq!(observed, vec![FsWatcherDriverEvent::ExternalEdit { path }]);
        drop(buffer);
    }
}
