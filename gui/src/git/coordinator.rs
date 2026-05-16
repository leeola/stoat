use crate::{
    buffer::Buffer,
    buffer_registry::{BufferRegistry, BufferRegistryEvent},
    executor::spawn_with_entity,
    git::blame::BlameState,
    globals::{ExecutorGlobal, GitHostGlobal},
};
use gpui::{AppContext, Context, Entity, Subscription, WeakEntity};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{buffer::BufferId, host::GitRepo};

/// Per-workspace orchestrator that owns one [`Entity<BlameState>`]
/// per [`BufferId`] so multiple editors on the same buffer share a
/// blame cache. Created lazily through [`Self::state_for`] and
/// refreshed on demand via [`Self::refresh`], which runs
/// [`GitRepo::blame_path`] on the executor and pushes the result
/// into the matching state on the gpui foreground.
///
/// State entries are dropped when [`BufferRegistry`] emits
/// [`BufferRegistryEvent::BufferRemoved`] so closing a buffer
/// releases its cache.
pub struct BlameCoordinator {
    git_root: PathBuf,
    states: HashMap<BufferId, Entity<BlameState>>,
    _registry_subscription: Subscription,
}

impl BlameCoordinator {
    pub fn new(
        git_root: PathBuf,
        registry: Entity<BufferRegistry>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let registry_subscription =
            cx.subscribe(&registry, |this, _, event: &BufferRegistryEvent, cx| {
                if let BufferRegistryEvent::BufferRemoved(id) = event {
                    if this.states.remove(id).is_some() {
                        cx.notify();
                    }
                }
            });
        Self {
            git_root,
            states: HashMap::new(),
            _registry_subscription: registry_subscription,
        }
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// Returns the [`Entity<BlameState>`] for `buffer_id`, creating
    /// a fresh empty state subscribed to `buffer` on first lookup.
    /// Subsequent calls with the same `buffer_id` return the cached
    /// entity even when `buffer` differs -- the registry's
    /// [`BufferRegistryEvent::BufferRemoved`] is the only signal
    /// that retires a state.
    pub fn state_for(
        &mut self,
        buffer_id: BufferId,
        buffer: Entity<Buffer>,
        cx: &mut Context<'_, Self>,
    ) -> Entity<BlameState> {
        if let Some(existing) = self.states.get(&buffer_id) {
            return existing.clone();
        }
        let state = cx.new(|cx| BlameState::new(buffer, cx));
        self.states.insert(buffer_id, state.clone());
        cx.notify();
        state
    }

    pub fn state_for_id(&self, buffer_id: BufferId) -> Option<&Entity<BlameState>> {
        self.states.get(&buffer_id)
    }

    pub fn tracked_count(&self) -> usize {
        self.states.len()
    }

    /// Run [`GitRepo::blame_path`] for `path` on the executor and
    /// push the result into the [`BlameState`] for `buffer_id`.
    /// No-op when no state is tracked for `buffer_id`, the workspace
    /// has no git repo, or `path` is not inside the workdir (the
    /// host method returns an empty `Vec` in those cases). The
    /// inner blame call blocks on libgit2 IO, so spawning is
    /// mandatory to keep the gpui foreground responsive.
    pub fn refresh(&self, buffer_id: BufferId, path: PathBuf, cx: &mut Context<'_, Self>) {
        let Some(state) = self.states.get(&buffer_id).cloned() else {
            return;
        };
        let git = cx.global::<GitHostGlobal>().0.clone();
        let Some(repo) = git.discover(&self.git_root) else {
            return;
        };
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let weak_state: WeakEntity<BlameState> = state.downgrade();
        spawn_with_entity(
            &executor,
            &cx.to_async(),
            weak_state,
            async move { fetch_blame(repo, path) },
            move |state, lines, cx| {
                state.set_blame(lines, cx);
            },
        )
        .detach();
    }
}

fn fetch_blame(repo: Arc<dyn GitRepo>, path: PathBuf) -> Vec<stoat::host::BlameLine> {
    repo.blame_path(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git::blame::BlameStateEvent,
        globals::{ExecutorGlobal, GitHostGlobal},
    };
    use gpui::{Subscription, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::host::{fake::FakeGit, BlameLine, GitHost};
    use stoat_scheduler::{Executor, TestScheduler};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            state: &Entity<BlameState>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<BlameStateEvent>>>) {
            let events: Arc<Mutex<Vec<BlameStateEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let state = state.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&state, move |_, _, event: &BlameStateEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<BlameStateEvent>>>) -> Vec<BlameStateEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn install_globals(cx: &mut TestAppContext, git: Arc<FakeGit>) -> Arc<TestScheduler> {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = Executor::new(scheduler.clone());
        cx.update(|cx| {
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
            cx.set_global(ExecutorGlobal(executor));
        });
        scheduler
    }

    fn settle(scheduler: &Arc<TestScheduler>, cx: &mut TestAppContext) {
        for _ in 0..4 {
            scheduler.run_until_parked();
            cx.run_until_parked();
        }
    }

    fn new_coordinator(
        cx: &mut TestAppContext,
        git_root: PathBuf,
    ) -> (Entity<BufferRegistry>, Entity<BlameCoordinator>) {
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let coordinator = {
            let registry = registry.clone();
            cx.update(|cx| cx.new(|cx| BlameCoordinator::new(git_root, registry, cx)))
        };
        (registry, coordinator)
    }

    fn new_buffer(cx: &mut TestAppContext, path: Option<PathBuf>, text: &str) -> Entity<Buffer> {
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(1), text));
            if let Some(p) = path {
                buffer.update(cx, |b, cx| b.set_file_path(Some(p), cx));
            }
            buffer
        })
    }

    #[test]
    fn new_coordinator_has_no_tracked_states() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));

        coordinator.read_with(&cx, |c, _| {
            assert_eq!(c.tracked_count(), 0);
            assert!(c.state_for_id(BufferId::new(1)).is_none());
        });
    }

    #[test]
    fn state_for_returns_same_entity_for_same_buffer() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, None, "hi");

        let first = coordinator.update(&mut cx, |c, cx| {
            c.state_for(BufferId::new(1), buffer.clone(), cx)
        });
        let second = coordinator.update(&mut cx, |c, cx| {
            c.state_for(BufferId::new(1), buffer.clone(), cx)
        });

        assert_eq!(first.entity_id(), second.entity_id());
        coordinator.read_with(&cx, |c, _| assert_eq!(c.tracked_count(), 1));
    }

    #[test]
    fn state_for_distinct_buffers_returns_distinct_entities() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let a = new_buffer(&mut cx, None, "a");
        let b = new_buffer(&mut cx, None, "b");

        let state_a = coordinator.update(&mut cx, |c, cx| c.state_for(BufferId::new(1), a, cx));
        let state_b = coordinator.update(&mut cx, |c, cx| c.state_for(BufferId::new(2), b, cx));

        assert_ne!(state_a.entity_id(), state_b.entity_id());
        coordinator.read_with(&cx, |c, _| assert_eq!(c.tracked_count(), 2));
    }

    #[test]
    fn refresh_pushes_blame_into_state() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo")
            .commit("c1", &[("a.rs", "line1\nline2\n")]);
        let scheduler = install_globals(&mut cx, git);
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/repo/a.rs")), "line1\nline2\n");

        let state = coordinator.update(&mut cx, |c, cx| c.state_for(BufferId::new(1), buffer, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        coordinator.update(&mut cx, |c, cx| {
            c.refresh(BufferId::new(1), PathBuf::from("/repo/a.rs"), cx)
        });
        settle(&scheduler, &mut cx);

        assert_eq!(drain(&events), vec![BlameStateEvent::Changed]);
        let lines: Vec<BlameLine> = state.read_with(&cx, |s, _| s.blame().to_vec());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 0);
        assert_eq!(lines[1].line, 1);
    }

    #[test]
    fn refresh_with_no_tracked_state_is_noop() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));

        coordinator.update(&mut cx, |c, cx| {
            c.refresh(BufferId::new(99), PathBuf::from("/repo/a.rs"), cx)
        });
        cx.run_until_parked();
        coordinator.read_with(&cx, |c, _| assert_eq!(c.tracked_count(), 0));
    }

    #[test]
    fn buffer_remove_event_drops_tracked_state() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let (registered_id, _shared) = registry.update(&mut cx, |r, cx| r.new_scratch(cx));
        let buffer = new_buffer(&mut cx, None, "x");
        coordinator.update(&mut cx, |c, cx| {
            c.state_for(registered_id, buffer, cx);
        });
        coordinator.read_with(&cx, |c, _| assert_eq!(c.tracked_count(), 1));

        registry.update(&mut cx, |r, cx| r.remove(registered_id, cx));
        cx.run_until_parked();

        coordinator.read_with(&cx, |c, _| {
            assert_eq!(c.tracked_count(), 0);
            assert!(c.state_for_id(registered_id).is_none());
        });
    }
}
