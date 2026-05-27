use crate::{
    fake::fs::FakeFs,
    watch::{FsEventKind, FsWatchEvent, FsWatchHost, WatchToken},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

/// In-memory [`FsWatchHost`] for tests. Events are produced either by
/// explicit calls to [`Self::inject`] or, when paired with a [`FakeFs`]
/// via [`Self::install_on`], automatically when [`FsHost::write`]
/// (`crate::fs::FsHost::write`) lands on a watched path.
pub struct FakeFsWatcher {
    state: Mutex<FakeWatchState>,
}

struct FakeWatchState {
    next_id: u64,
    tokens: BTreeMap<WatchToken, PathBuf>,
    paths: BTreeMap<PathBuf, Vec<WatchToken>>,
    queue: VecDeque<FsWatchEvent>,
}

impl Default for FakeFsWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeFsWatcher {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeWatchState {
                next_id: 0,
                tokens: BTreeMap::new(),
                paths: BTreeMap::new(),
                queue: VecDeque::new(),
            }),
        }
    }

    /// Push an event onto the queue for `path` regardless of whether
    /// the path is currently watched. Tests that want the watcher to
    /// behave like a paired [`FakeFs`] write should prefer
    /// [`Self::install_on`].
    pub fn inject(&self, path: impl AsRef<Path>, kind: FsEventKind) {
        let mut state = self.state.lock().expect("FakeFsWatcher poisoned");
        state.queue.push_back(FsWatchEvent {
            path: path.as_ref().to_path_buf(),
            kind,
        });
    }

    /// Snapshot of currently-watched paths. Each path appears once
    /// regardless of how many tokens reference it.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        let state = self.state.lock().expect("FakeFsWatcher poisoned");
        state.paths.keys().cloned().collect()
    }

    /// Whether `path` is currently watched by at least one token.
    pub fn is_watching(&self, path: &Path) -> bool {
        let state = self.state.lock().expect("FakeFsWatcher poisoned");
        state.paths.contains_key(path)
    }

    /// Wire `fs` so every successful [`FsHost::write`]
    /// (`crate::fs::FsHost::write`) on a currently-watched path
    /// auto-emits a [`FsEventKind::Modified`] event. Holds a weak
    /// reference to `self`, so dropping the watcher disarms the hook
    /// silently.
    pub fn install_on(self: &Arc<Self>, fs: &FakeFs) {
        let weak = Arc::downgrade(self);
        fs.set_write_hook(Box::new(move |path: &Path| {
            let Some(watcher) = Weak::upgrade(&weak) else {
                return;
            };
            if watcher.is_watching(path) {
                watcher.inject(path, FsEventKind::Modified);
            }
        }));
    }
}

impl FsWatchHost for FakeFsWatcher {
    fn watch(&self, path: &Path) -> io::Result<WatchToken> {
        let mut state = self.state.lock().expect("FakeFsWatcher poisoned");
        let token = WatchToken(state.next_id);
        state.next_id += 1;
        state.tokens.insert(token, path.to_path_buf());
        state
            .paths
            .entry(path.to_path_buf())
            .or_default()
            .push(token);
        Ok(token)
    }

    fn watch_dir(&self, path: &Path) -> io::Result<WatchToken> {
        let mut state = self.state.lock().expect("FakeFsWatcher poisoned");
        let token = WatchToken(state.next_id);
        state.next_id += 1;
        state.tokens.insert(token, path.to_path_buf());
        state
            .paths
            .entry(path.to_path_buf())
            .or_default()
            .push(token);
        Ok(token)
    }

    fn unwatch(&self, token: WatchToken) {
        let mut state = self.state.lock().expect("FakeFsWatcher poisoned");
        let Some(path) = state.tokens.remove(&token) else {
            return;
        };
        let now_empty = match state.paths.get_mut(&path) {
            Some(tokens) => {
                tokens.retain(|t| *t != token);
                tokens.is_empty()
            },
            None => false,
        };
        if now_empty {
            state.paths.remove(&path);
        }
    }

    fn try_recv(&self) -> Option<FsWatchEvent> {
        self.state
            .lock()
            .expect("FakeFsWatcher poisoned")
            .queue
            .pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FsHost;

    #[test]
    fn inject_and_drain() {
        let watcher = FakeFsWatcher::new();
        watcher.inject("/a", FsEventKind::Modified);
        watcher.inject("/b", FsEventKind::Created);
        assert_eq!(
            watcher.try_recv(),
            Some(FsWatchEvent {
                path: PathBuf::from("/a"),
                kind: FsEventKind::Modified,
            }),
        );
        assert_eq!(
            watcher.try_recv(),
            Some(FsWatchEvent {
                path: PathBuf::from("/b"),
                kind: FsEventKind::Created,
            }),
        );
        assert_eq!(watcher.try_recv(), None);
    }

    #[test]
    fn watch_records_path() {
        let watcher = FakeFsWatcher::new();
        watcher.watch(Path::new("/x")).unwrap();
        watcher.watch(Path::new("/y")).unwrap();
        let mut paths = watcher.watched_paths();
        paths.sort();
        assert_eq!(paths, [PathBuf::from("/x"), PathBuf::from("/y")]);
    }

    #[test]
    fn watch_dir_records_path() {
        let watcher = FakeFsWatcher::new();
        watcher.watch_dir(Path::new("/workdir")).unwrap();
        assert_eq!(watcher.watched_paths(), [PathBuf::from("/workdir")]);
    }

    #[test]
    fn unwatch_removes_only_matching_token() {
        let watcher = FakeFsWatcher::new();
        let t1 = watcher.watch(Path::new("/x")).unwrap();
        let t2 = watcher.watch(Path::new("/x")).unwrap();
        assert_ne!(t1, t2);
        watcher.unwatch(t1);
        assert!(watcher.is_watching(Path::new("/x")));
        watcher.unwatch(t2);
        assert!(!watcher.is_watching(Path::new("/x")));
    }

    #[test]
    fn unwatch_unknown_token_is_noop() {
        let watcher = FakeFsWatcher::new();
        watcher.unwatch(WatchToken(99));
        let token = watcher.watch(Path::new("/x")).unwrap();
        watcher.unwatch(token);
        assert!(watcher.watched_paths().is_empty());
    }

    #[test]
    fn install_on_emits_modified_for_watched_writes() {
        let fs = FakeFs::new();
        let watcher = Arc::new(FakeFsWatcher::new());
        watcher.install_on(&fs);
        watcher.watch(Path::new("/a/b.txt")).unwrap();

        fs.write(Path::new("/a/b.txt"), b"hello").unwrap();
        assert_eq!(
            watcher.try_recv(),
            Some(FsWatchEvent {
                path: PathBuf::from("/a/b.txt"),
                kind: FsEventKind::Modified,
            }),
        );
        assert_eq!(watcher.try_recv(), None);
    }

    #[test]
    fn install_on_skips_unwatched_writes() {
        let fs = FakeFs::new();
        let watcher = Arc::new(FakeFsWatcher::new());
        watcher.install_on(&fs);

        fs.write(Path::new("/untracked"), b"hi").unwrap();
        assert_eq!(watcher.try_recv(), None);
    }

    #[test]
    fn install_on_skips_after_watcher_dropped() {
        let fs = FakeFs::new();
        let watcher = Arc::new(FakeFsWatcher::new());
        watcher.install_on(&fs);
        watcher.watch(Path::new("/a")).unwrap();
        drop(watcher);
        fs.write(Path::new("/a"), b"x").unwrap();
        // Hook upgrades a Weak; once the last Arc is dropped the hook
        // becomes a no-op without crashing FakeFs::write.
    }

    #[test]
    fn write_failure_skips_hook() {
        use std::io;
        let fs = FakeFs::new();
        let watcher = Arc::new(FakeFsWatcher::new());
        watcher.install_on(&fs);
        watcher.watch(Path::new("/locked")).unwrap();
        fs.fail_writes_to("/locked", io::ErrorKind::PermissionDenied);
        let _ = fs.write(Path::new("/locked"), b"x");
        assert_eq!(watcher.try_recv(), None);
    }
}
