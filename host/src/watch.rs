use notify::{
    event::{EventKind as NotifyEventKind, ModifyKind},
    RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    collections::{BTreeMap, VecDeque},
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

    /// Begin watching `path` and every descendant recursively. Events
    /// surface with the full path of the affected file in
    /// [`FsWatchEvent::path`], not the watched directory. Mixing
    /// [`Self::watch`] and [`Self::watch_dir`] on the *same* path is
    /// undefined; production callers watch files and directories at
    /// distinct paths.
    fn watch_dir(&self, path: &Path) -> io::Result<WatchToken>;

    /// Drop the watch for `token`. Tokens from another host or
    /// already-released tokens are silently ignored.
    fn unwatch(&self, token: WatchToken);

    /// Pop one queued event, or `None` if the queue is empty. Drives
    /// from a synchronous polling site (e.g. the editor's per-tick
    /// drain loop); does not block.
    fn try_recv(&self) -> Option<FsWatchEvent>;
}

/// Production [`FsWatchHost`] backed by [`notify::RecommendedWatcher`].
///
/// notify's recommended watcher spawns its own platform-specific
/// background thread (FSEvents on macOS, inotify on Linux,
/// ReadDirectoryChangesW on Windows); the closure passed to
/// [`notify::recommended_watcher`] runs there and pushes onto the
/// shared queue drained by [`Self::try_recv`].
pub struct LocalFsWatcher {
    inner: Mutex<LocalFsWatcherInner>,
    queue: Arc<Mutex<VecDeque<FsWatchEvent>>>,
}

struct LocalFsWatcherInner {
    watcher: RecommendedWatcher,
    next_id: u64,
    tokens: BTreeMap<WatchToken, PathBuf>,
    refs: BTreeMap<PathBuf, usize>,
}

impl LocalFsWatcher {
    pub fn new() -> io::Result<Self> {
        let queue: Arc<Mutex<VecDeque<FsWatchEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue_for_handler = queue.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                return;
            };
            let Some(kind) = translate_event_kind(&event.kind) else {
                return;
            };
            let mut q = queue_for_handler
                .lock()
                .expect("LocalFsWatcher queue poisoned");
            for path in event.paths {
                q.push_back(FsWatchEvent { path, kind });
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
        })
    }
}

impl FsWatchHost for LocalFsWatcher {
    fn watch(&self, path: &Path) -> io::Result<WatchToken> {
        let mut inner = self.inner.lock().expect("LocalFsWatcher poisoned");
        let prior = inner.refs.get(path).copied().unwrap_or(0);
        if prior == 0 {
            inner
                .watcher
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(notify_to_io)?;
        }
        inner.refs.insert(path.to_path_buf(), prior + 1);
        let token = WatchToken(inner.next_id);
        inner.next_id += 1;
        inner.tokens.insert(token, path.to_path_buf());
        Ok(token)
    }

    fn watch_dir(&self, path: &Path) -> io::Result<WatchToken> {
        let mut inner = self.inner.lock().expect("LocalFsWatcher poisoned");
        let prior = inner.refs.get(path).copied().unwrap_or(0);
        if prior == 0 {
            inner
                .watcher
                .watch(path, RecursiveMode::Recursive)
                .map_err(notify_to_io)?;
        }
        inner.refs.insert(path.to_path_buf(), prior + 1);
        let token = WatchToken(inner.next_id);
        inner.next_id += 1;
        inner.tokens.insert(token, path.to_path_buf());
        Ok(token)
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
            .pop_front()
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

    fn watch_dir(&self, _path: &Path) -> io::Result<WatchToken> {
        let mut next = self.next_id.lock().expect("NoopFsWatcher poisoned");
        let token = WatchToken(*next);
        *next += 1;
        Ok(token)
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
