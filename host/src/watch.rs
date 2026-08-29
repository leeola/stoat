use notify::{
    event::{EventKind as NotifyEventKind, ModifyKind},
    RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WatchToken(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FsEventKind {
    Modified,
    Created,
    Removed,
    Renamed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsWatchEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

/// The callback a host runs when its queue goes from empty to occupied,
/// installed through [`FsWatchHost::set_wake`].
pub type WakeFn = Box<dyn Fn() + Send + Sync>;

/// Filesystem-change subscription, modelled as a queue the host fills
/// in the background and the application drains synchronously.
///
/// Implementations may collapse repeated events for a single path into
/// fewer queue entries (notify backends differ in granularity); callers
/// must not assume one production-side write yields exactly one event.
/// Watching the same path twice and then unwatching one of the
/// resulting tokens leaves the remaining watch in place.
pub trait FsWatchHost: Send + Sync {
    /// Begin watching `path` non-recursively. The returned token
    /// identifies the watch for [`Self::unwatch`]; ignoring it leaves
    /// the watch active for the host's lifetime.
    fn watch(&self, path: &Path) -> io::Result<WatchToken>;

    /// Begin watching `path` and everything beneath it. Like
    /// [`Self::watch`] but recursive, for subscribing to a whole
    /// directory tree from its root in a single call.
    fn watch_recursive(&self, path: &Path) -> io::Result<WatchToken>;

    /// Drop the watch for `token`. Tokens from another host or
    /// already-released tokens are silently ignored.
    fn unwatch(&self, token: WatchToken);

    /// Pop one queued event, or `None` if the queue is empty. Drives
    /// from a synchronous polling site (e.g. the editor's per-tick
    /// drain loop); does not block.
    fn try_recv(&self) -> Option<FsWatchEvent>;

    /// Install a callback the host runs when the queue goes from empty
    /// to occupied.
    ///
    /// A polling drain sees events only when something else already woke
    /// the caller. An editor that nobody types into therefore accumulates
    /// a checkout's worth of events until the next keystroke. The wake is
    /// what lets the caller drain while the user is idle.
    ///
    /// Runs on the host's background thread, so it must not block. Hosts
    /// that never produce events ignore it.
    fn set_wake(&self, wake: WakeFn) {
        let _ = wake;
    }
}

/// Fold `next` into the kind already queued for a path.
///
/// A creation followed by a modification stays [`FsEventKind::Created`].
/// The caller acts on the creation itself, since a new directory needs its
/// own watch. A merged event reported as a modification leaves that
/// directory unwatched.
///
/// Every other pair takes the later kind, which is the one that describes
/// the file's present state.
pub fn merge_event_kind(prev: Option<FsEventKind>, next: FsEventKind) -> FsEventKind {
    match (prev, next) {
        (Some(FsEventKind::Created), FsEventKind::Modified) => FsEventKind::Created,
        _ => next,
    }
}

/// Production [`FsWatchHost`] backed by [`notify::RecommendedWatcher`].
///
/// notify's recommended watcher spawns its own platform-specific
/// background thread (FSEvents on macOS, inotify on Linux,
/// ReadDirectoryChangesW on Windows); the closure passed to
/// [`notify::recommended_watcher`] runs there and merges into the
/// shared map drained by [`Self::try_recv`].
pub struct LocalFsWatcher {
    inner: Mutex<LocalFsWatcherInner>,
    /// One entry per path, so a burst of writes to a file costs the drain
    /// one event however many the platform reports. Ordered so the drain
    /// walks a directory's files together rather than in arrival order.
    queue: Arc<Mutex<BTreeMap<PathBuf, FsEventKind>>>,
    wake: Arc<Mutex<Option<WakeFn>>>,
}

struct LocalFsWatcherInner {
    watcher: RecommendedWatcher,
    next_id: u64,
    tokens: BTreeMap<WatchToken, PathBuf>,
    refs: BTreeMap<PathBuf, usize>,
}

impl LocalFsWatcher {
    pub fn new() -> io::Result<Self> {
        let queue: Arc<Mutex<BTreeMap<PathBuf, FsEventKind>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let wake: Arc<Mutex<Option<WakeFn>>> = Arc::new(Mutex::new(None));

        let queue_for_handler = queue.clone();
        let wake_for_handler = wake.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                return;
            };
            let Some(kind) = translate_event_kind(&event.kind) else {
                return;
            };

            let was_empty = {
                let mut q = queue_for_handler
                    .lock()
                    .expect("LocalFsWatcher queue poisoned");
                let was_empty = q.is_empty();
                for path in event.paths {
                    let merged = merge_event_kind(q.get(&path).copied(), kind);
                    q.insert(path, merged);
                }
                was_empty && !q.is_empty()
            };

            // Only the empty-to-occupied edge, so a burst wakes the drain
            // once rather than per event. The drain empties the map, which
            // is what arms the next edge.
            if was_empty
                && let Some(wake) = wake_for_handler
                    .lock()
                    .expect("LocalFsWatcher wake poisoned")
                    .as_ref()
            {
                wake();
            }
        })
        .map_err(notify_to_io)?;

        Ok(Self {
            inner: Mutex::new(LocalFsWatcherInner {
                watcher,
                next_id: 0,
                tokens: BTreeMap::new(),
                refs: BTreeMap::new(),
            }),
            queue,
            wake,
        })
    }

    fn watch_with_mode(&self, path: &Path, mode: RecursiveMode) -> io::Result<WatchToken> {
        let mut inner = self.inner.lock().expect("LocalFsWatcher poisoned");
        let prior = inner.refs.get(path).copied().unwrap_or(0);
        if prior == 0 {
            inner.watcher.watch(path, mode).map_err(notify_to_io)?;
        }
        inner.refs.insert(path.to_path_buf(), prior + 1);
        let token = WatchToken(inner.next_id);
        inner.next_id += 1;
        inner.tokens.insert(token, path.to_path_buf());
        Ok(token)
    }
}

impl FsWatchHost for LocalFsWatcher {
    fn watch(&self, path: &Path) -> io::Result<WatchToken> {
        self.watch_with_mode(path, RecursiveMode::NonRecursive)
    }

    fn watch_recursive(&self, path: &Path) -> io::Result<WatchToken> {
        self.watch_with_mode(path, RecursiveMode::Recursive)
    }

    fn unwatch(&self, token: WatchToken) {
        let mut inner = self.inner.lock().expect("LocalFsWatcher poisoned");
        let Some(path) = inner.tokens.remove(&token) else {
            return;
        };
        let drop_path = match inner.refs.get_mut(&path) {
            Some(count) => {
                *count = count.saturating_sub(1);
                *count == 0
            },
            None => false,
        };
        if drop_path {
            inner.refs.remove(&path);
            let _ = inner.watcher.unwatch(&path);
        }
    }

    fn try_recv(&self) -> Option<FsWatchEvent> {
        self.queue
            .lock()
            .expect("LocalFsWatcher queue poisoned")
            .pop_first()
            .map(|(path, kind)| FsWatchEvent { path, kind })
    }

    fn set_wake(&self, wake: WakeFn) {
        *self.wake.lock().expect("LocalFsWatcher wake poisoned") = Some(wake);
    }
}

/// Zero-event [`FsWatchHost`]. `watch` and `unwatch` are silent; `try_recv`
/// always returns `None`. Used as the default registered on
/// `Stoat::new` so the editor constructs without fallible IO; the
/// bin layer swaps in [`LocalFsWatcher`] and tests swap in
/// [`crate::FakeFsWatcher`].
pub struct NoopFsWatcher {
    next_id: Mutex<u64>,
}

impl Default for NoopFsWatcher {
    fn default() -> Self {
        Self {
            next_id: Mutex::new(0),
        }
    }
}

impl NoopFsWatcher {
    pub fn new() -> Self {
        Self::default()
    }
}

impl FsWatchHost for NoopFsWatcher {
    fn watch(&self, _path: &Path) -> io::Result<WatchToken> {
        let mut next = self.next_id.lock().expect("NoopFsWatcher poisoned");
        let token = WatchToken(*next);
        *next += 1;
        Ok(token)
    }

    fn watch_recursive(&self, path: &Path) -> io::Result<WatchToken> {
        self.watch(path)
    }

    fn unwatch(&self, _token: WatchToken) {}

    fn try_recv(&self) -> Option<FsWatchEvent> {
        None
    }
}

/// Map a [`notify::Event`] kind onto the smaller [`FsEventKind`] surface.
/// Returns `None` for events we don't propagate (`Access`, `Any`,
/// `Other`); access events especially are noise on Linux.
fn translate_event_kind(kind: &NotifyEventKind) -> Option<FsEventKind> {
    match kind {
        NotifyEventKind::Create(_) => Some(FsEventKind::Created),
        NotifyEventKind::Remove(_) => Some(FsEventKind::Removed),
        NotifyEventKind::Modify(ModifyKind::Name(_)) => Some(FsEventKind::Renamed),
        NotifyEventKind::Modify(_) => Some(FsEventKind::Modified),
        NotifyEventKind::Access(_) | NotifyEventKind::Any | NotifyEventKind::Other => None,
    }
}

fn notify_to_io(err: notify::Error) -> io::Error {
    io::Error::other(format!("notify: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, RemoveKind, RenameMode};

    #[test]
    fn translates_notify_kinds_to_fs_events() {
        let cases = [
            (
                NotifyEventKind::Create(CreateKind::File),
                Some(FsEventKind::Created),
            ),
            (
                NotifyEventKind::Remove(RemoveKind::File),
                Some(FsEventKind::Removed),
            ),
            (
                NotifyEventKind::Modify(ModifyKind::Name(RenameMode::To)),
                Some(FsEventKind::Renamed),
            ),
            (
                NotifyEventKind::Modify(ModifyKind::Data(DataChange::Content)),
                Some(FsEventKind::Modified),
            ),
            (
                NotifyEventKind::Modify(ModifyKind::Any),
                Some(FsEventKind::Modified),
            ),
            (NotifyEventKind::Access(AccessKind::Read), None),
            (NotifyEventKind::Any, None),
            (NotifyEventKind::Other, None),
        ];
        for (kind, expected) in cases {
            assert_eq!(translate_event_kind(&kind), expected, "kind: {kind:?}");
        }
    }

    #[test]
    fn merging_keeps_a_creation_and_otherwise_takes_the_later_kind() {
        use FsEventKind::{Created, Modified, Removed, Renamed};

        let cases = [
            (None, Modified, Modified),
            (None, Created, Created),
            // The one pair where the later kind loses. A caller that acts
            // on the creation still has to see it.
            (Some(Created), Modified, Created),
            (Some(Created), Removed, Removed),
            (Some(Created), Renamed, Renamed),
            (Some(Created), Created, Created),
            (Some(Modified), Modified, Modified),
            (Some(Modified), Created, Created),
            (Some(Modified), Removed, Removed),
            (Some(Removed), Created, Created),
            (Some(Renamed), Modified, Modified),
        ];
        for (prev, next, expected) in cases {
            assert_eq!(
                merge_event_kind(prev, next),
                expected,
                "prev: {prev:?}, next: {next:?}",
            );
        }
    }
}
