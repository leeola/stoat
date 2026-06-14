use gpui::{Context, EventEmitter};
use std::path::Path;
use stoat::{
    buffer::{BufferId, SharedBuffer},
    buffer_registry::{BufferRegistrySnapshot, DirtyBuffer},
    BufferRegistry as InnerRegistry,
};

/// Entity-shaped wrapper around [`stoat::BufferRegistry`]. Mutations go
/// through this wrapper so the entity emits [`BufferRegistryEvent`]s on
/// the gpui foreground; the buffer-list pane and any other observer
/// re-renders without polling.
pub struct BufferRegistry {
    inner: InnerRegistry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferRegistryEvent {
    BufferAdded(BufferId),
    BufferRemoved(BufferId),
}

impl EventEmitter<BufferRegistryEvent> for BufferRegistry {}

impl BufferRegistry {
    pub fn new() -> Self {
        Self {
            inner: InnerRegistry::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn new_scratch(&mut self, cx: &mut Context<'_, Self>) -> (BufferId, SharedBuffer) {
        let result = self.inner.new_scratch();
        cx.emit(BufferRegistryEvent::BufferAdded(result.0));
        cx.notify();
        result
    }

    /// Allocate a scratch buffer seeded with `text` (e.g. piped stdin).
    pub fn new_scratch_with_text(
        &mut self,
        text: &str,
        cx: &mut Context<'_, Self>,
    ) -> (BufferId, SharedBuffer) {
        let result = self.inner.new_scratch_with_text(text);
        cx.emit(BufferRegistryEvent::BufferAdded(result.0));
        cx.notify();
        result
    }

    /// Open `path`, returning the existing buffer when one is already
    /// registered for that path. Emits [`BufferRegistryEvent::BufferAdded`]
    /// only when a new entry was allocated.
    pub fn open(
        &mut self,
        path: &Path,
        text: &str,
        cx: &mut Context<'_, Self>,
    ) -> (BufferId, SharedBuffer) {
        let was_known = self.inner.id_for_path(path).is_some();
        let result = self.inner.open(path, text);
        if !was_known {
            cx.emit(BufferRegistryEvent::BufferAdded(result.0));
            cx.notify();
        }
        result
    }

    pub fn remove(&mut self, id: BufferId, cx: &mut Context<'_, Self>) {
        if self.inner.get(id).is_none() {
            return;
        }
        self.inner.remove(id);
        cx.emit(BufferRegistryEvent::BufferRemoved(id));
        cx.notify();
    }

    pub fn get(&self, id: BufferId) -> Option<SharedBuffer> {
        self.inner.get(id)
    }

    pub fn id_for_path(&self, path: &Path) -> Option<BufferId> {
        self.inner.id_for_path(path)
    }

    pub fn path_for(&self, id: BufferId) -> Option<&Path> {
        self.inner.path_for(id)
    }

    /// Iterate the open [`BufferId`]s. Order is not stable across
    /// runs (see [`stoat::BufferRegistry::ids`]); callers that need
    /// deterministic ordering sort by path or another orthogonal
    /// key.
    pub fn ids(&self) -> impl Iterator<Item = BufferId> + '_ {
        self.inner.ids()
    }

    /// Every buffer whose dirty flag is set. Order: path-bound
    /// first sorted by path, scratch buffers after sorted by id.
    pub fn dirty_buffers(&self) -> Vec<DirtyBuffer> {
        self.inner.dirty_buffers()
    }

    /// Snapshot every buffer's history for workspace persistence.
    /// Forwards to [`stoat::BufferRegistry::snapshot`]; scratch and
    /// path-bound entries are both captured so the op-log round-trips
    /// across restart via [`stoat::buffer::TextBuffer::from_history`].
    pub fn snapshot(&self) -> BufferRegistrySnapshot {
        self.inner.snapshot()
    }

    /// Replace the registry contents with `snap`, emitting one
    /// [`BufferRegistryEvent::BufferAdded`] per rehydrated entry so
    /// existing subscribers (file-watcher driver, buffer picker)
    /// see the restored set. Must be paired with a workspace state
    /// restore that re-installs the matching editor entities; calling
    /// this in isolation leaves the registry populated but with no
    /// panes referencing the buffers.
    pub fn restore_from(&mut self, snap: BufferRegistrySnapshot, cx: &mut Context<'_, Self>) {
        let ids: Vec<BufferId> = snap.entries.iter().map(|e| e.id).collect();
        self.inner.restore_from(snap);
        for id in ids {
            cx.emit(BufferRegistryEvent::BufferAdded(id));
        }
        cx.notify();
    }
}

impl Default for BufferRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            registry: &Entity<BufferRegistry>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<BufferRegistryEvent>>>) {
            let events: Arc<Mutex<Vec<BufferRegistryEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let registry = registry.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&registry, move |_, _, event: &BufferRegistryEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn new_registry(cx: &mut TestAppContext) -> Entity<BufferRegistry> {
        cx.update(|cx| cx.new(|_| BufferRegistry::new()))
    }

    fn drain(events: &Arc<Mutex<Vec<BufferRegistryEvent>>>) -> Vec<BufferRegistryEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    #[test]
    fn new_scratch_emits_buffer_added() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &registry);

        let id = registry.update(&mut cx, |r, cx| r.new_scratch(cx).0);
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferRegistryEvent::BufferAdded(id)]);
        assert_eq!(registry.read_with(&cx, |r, _| r.len()), 1);
    }

    #[test]
    fn open_new_path_emits_buffer_added() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &registry);

        let id = registry.update(&mut cx, |r, cx| r.open(Path::new("/a.txt"), "hi", cx).0);
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferRegistryEvent::BufferAdded(id)]);
    }

    #[test]
    fn open_known_path_does_not_emit() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (_first_id, second_id) = registry.update(&mut cx, |r, cx| {
            let first = r.open(Path::new("/a.txt"), "hi", cx).0;
            let second = r.open(Path::new("/a.txt"), "ignored", cx).0;
            (first, second)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &registry);
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BufferRegistryEvent>::new());
        assert_eq!(
            registry.read_with(&cx, |r, _| r.id_for_path(Path::new("/a.txt"))),
            Some(second_id)
        );
    }

    #[test]
    fn remove_known_emits_buffer_removed() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let id = registry.update(&mut cx, |r, cx| r.new_scratch(cx).0);
        let (_recorder, events) = Recorder::install(&mut cx, &registry);

        registry.update(&mut cx, |r, cx| r.remove(id, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BufferRegistryEvent::BufferRemoved(id)]);
        assert_eq!(registry.read_with(&cx, |r, _| r.len()), 0);
    }

    #[test]
    fn remove_unknown_does_not_emit() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &registry);

        registry.update(&mut cx, |r, cx| r.remove(BufferId::new(42), cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BufferRegistryEvent>::new());
    }

    #[test]
    fn ids_iterates_open_buffers() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (a, b) = registry.update(&mut cx, |r, cx| {
            let a = r.open(Path::new("/a.txt"), "", cx).0;
            let b = r.open(Path::new("/b.txt"), "", cx).0;
            (a, b)
        });
        let mut ids: Vec<_> = registry.read_with(&cx, |r, _| r.ids().collect());
        ids.sort();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn path_for_returns_known_path() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let id = registry.update(&mut cx, |r, cx| r.open(Path::new("/x.rs"), "", cx).0);
        let path = registry.read_with(&cx, |r, _| r.path_for(id).map(|p| p.to_path_buf()));
        assert_eq!(path, Some(PathBuf::from("/x.rs")));
    }

    #[test]
    fn path_for_returns_none_for_scratch() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let id = registry.update(&mut cx, |r, cx| r.new_scratch(cx).0);
        let path = registry.read_with(&cx, |r, _| r.path_for(id).map(|p| p.to_path_buf()));
        assert_eq!(path, None);
    }

    #[test]
    fn get_returns_shared_buffer_after_add() {
        let mut cx = TestAppContext::single();
        let registry = new_registry(&mut cx);
        let (id, shared) =
            registry.update(&mut cx, |r, cx| r.open(Path::new("/x.txt"), "hello", cx));

        let fetched = registry
            .read_with(&cx, |r, _| r.get(id))
            .expect("get returns shared buffer");
        assert!(Arc::ptr_eq(&shared, &fetched));
        assert_eq!(
            fetched
                .read()
                .expect("buffer lock poisoned")
                .rope()
                .to_string(),
            "hello"
        );
    }
}
