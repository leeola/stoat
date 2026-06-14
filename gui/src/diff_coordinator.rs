use crate::{
    buffer::{Buffer, BufferEvent},
    buffer_registry::{BufferRegistry, BufferRegistryEvent},
    diff_map::DiffMap,
    globals::{GitHostGlobal, LanguageRegistry},
};
use gpui::{AppContext, Context, Entity, Subscription, WeakEntity};
use std::{collections::HashMap, path::PathBuf};
use stoat::{buffer::BufferId, DiffMap as InnerDiffMap};
use stoat_language::structural_diff;

/// Per-workspace orchestrator that recomputes the working-tree diff
/// for tracked buffers and writes the result into a per-buffer
/// [`Entity<DiffMap>`]. Editors fetch the DiffMap entity for their
/// buffer via [`DiffCoordinator::diff_map_for`] so the gutter strip
/// renders the same diff data the coordinator computed.
///
/// Computation runs on every [`BufferEvent::Edited`],
/// [`BufferEvent::Reloaded`], or [`BufferEvent::LanguageChanged`]
/// for tracked buffers; the diff is built by reading HEAD content
/// via the workspace's git host and running
/// [`structural_diff::diff_with_language_or_lines`] (or
/// [`structural_diff::diff`] when the file has no language).
///
/// Buffers are tracked explicitly via [`Self::track_buffer`]; the
/// coordinator also subscribes to [`BufferRegistry`] so removed
/// buffers drop their tracking automatically. Buffers without a
/// file path produce no diff (their DiffMap stays empty).
pub struct DiffCoordinator {
    git_root: PathBuf,
    buffers: HashMap<BufferId, BufferTracking>,
    _registry_subscription: Subscription,
}

struct BufferTracking {
    buffer: WeakEntity<Buffer>,
    diff_map: Entity<DiffMap>,
    _buffer_subscription: Subscription,
}

impl DiffCoordinator {
    pub fn new(
        git_root: PathBuf,
        registry: Entity<BufferRegistry>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let registry_subscription =
            cx.subscribe(&registry, |this, _, event: &BufferRegistryEvent, cx| {
                if let BufferRegistryEvent::BufferRemoved(id) = event
                    && this.buffers.remove(id).is_some()
                {
                    cx.notify();
                }
            });
        Self {
            git_root,
            buffers: HashMap::new(),
            _registry_subscription: registry_subscription,
        }
    }

    /// Begin tracking `buffer` under `buffer_id`. The coordinator
    /// allocates a fresh [`Entity<DiffMap>`] for the buffer, subscribes
    /// to its edit events, and runs an initial diff. Repeat calls for
    /// the same `buffer_id` are no-ops -- callers that want to retarget
    /// must remove the buffer from the registry first.
    pub fn track_buffer(
        &mut self,
        buffer_id: BufferId,
        buffer: Entity<Buffer>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.buffers.contains_key(&buffer_id) {
            return;
        }
        let diff_map = {
            let buffer = buffer.clone();
            cx.new(|cx| DiffMap::new(buffer, cx))
        };
        let weak_buffer = buffer.downgrade();
        let buffer_subscription = cx.subscribe(&buffer, move |this, _, event: &BufferEvent, cx| {
            if matches!(
                event,
                BufferEvent::Edited | BufferEvent::Reloaded | BufferEvent::LanguageChanged
            ) {
                this.recompute_for(buffer_id, cx);
            }
        });
        self.buffers.insert(
            buffer_id,
            BufferTracking {
                buffer: weak_buffer,
                diff_map,
                _buffer_subscription: buffer_subscription,
            },
        );
        self.recompute_for(buffer_id, cx);
        cx.notify();
    }

    pub fn diff_map_for(&self, buffer_id: BufferId) -> Option<&Entity<DiffMap>> {
        self.buffers.get(&buffer_id).map(|t| &t.diff_map)
    }

    pub fn tracked_count(&self) -> usize {
        self.buffers.len()
    }

    /// Point the coordinator at a new workspace root and recompute the
    /// working-tree diff for every tracked buffer against it, so the
    /// gutter reflects the new repository's HEAD.
    pub fn set_git_root(&mut self, git_root: PathBuf, cx: &mut Context<'_, Self>) {
        self.git_root = git_root;
        let ids: Vec<BufferId> = self.buffers.keys().copied().collect();
        for id in ids {
            self.recompute_for(id, cx);
        }
        cx.notify();
    }

    fn recompute_for(&mut self, buffer_id: BufferId, cx: &mut Context<'_, Self>) {
        let Some(tracking) = self.buffers.get(&buffer_id) else {
            return;
        };
        let Some(buffer) = tracking.buffer.upgrade() else {
            return;
        };
        let diff_map = tracking.diff_map.clone();

        let (file_path, buffer_text) = buffer.read_with(cx, |b, _| {
            (b.file_path().map(|p| p.to_path_buf()), b.text())
        });
        let Some(file_path) = file_path else {
            return;
        };

        let git = cx.global::<GitHostGlobal>().0.clone();
        let language = cx
            .try_global::<LanguageRegistry>()
            .and_then(|langs| langs.0.for_path(&file_path));

        let Some(repo) = git.discover(&self.git_root) else {
            return;
        };
        let base_text = repo.head_content(&file_path).unwrap_or_default();

        let result = match &language {
            Some(lang) => {
                structural_diff::diff_with_language_or_lines(lang, &base_text, &buffer_text)
            },
            None => structural_diff::diff(&base_text, &buffer_text),
        };
        let new_dm = InnerDiffMap::from_structural_changes(result, &base_text, &buffer_text);

        diff_map.update(cx, |dm, cx| dm.set_diff(new_dm, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMapEvent,
        globals::{GitHostGlobal, LanguageRegistry},
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::host::{fake::FakeGit, GitHost};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            diff_map: &Entity<DiffMap>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<DiffMapEvent>>>) {
            let events: Arc<Mutex<Vec<DiffMapEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let diff_map = diff_map.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&diff_map, move |_, _, event: &DiffMapEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<DiffMapEvent>>>) -> Vec<DiffMapEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn install_globals(cx: &mut TestAppContext, git: Arc<FakeGit>) {
        cx.update(|cx| {
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
            cx.set_global(LanguageRegistry::standard());
        });
    }

    fn new_coordinator(
        cx: &mut TestAppContext,
        git_root: PathBuf,
    ) -> (Entity<BufferRegistry>, Entity<DiffCoordinator>) {
        let registry = cx.update(|cx| cx.new(|_| BufferRegistry::new()));
        let coordinator = {
            let registry = registry.clone();
            cx.update(|cx| cx.new(|cx| DiffCoordinator::new(git_root, registry, cx)))
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
    fn new_coordinator_tracks_no_buffers() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));

        coordinator.read_with(&cx, |c, _| {
            assert_eq!(c.tracked_count(), 0);
            assert!(c.diff_map_for(BufferId::new(1)).is_none());
        });
    }

    #[test]
    fn track_buffer_creates_diff_map_and_runs_initial_compute() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").head_file("a.rs", "old\n");
        install_globals(&mut cx, git);
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/repo/a.rs")), "new\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer, cx)
        });
        cx.run_until_parked();

        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map for tracked buffer");
        assert!(!dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn buffer_edit_triggers_recompute_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").head_file("a.rs", "v1\n");
        install_globals(&mut cx, git);
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/repo/a.rs")), "v1\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer.clone(), cx)
        });
        cx.run_until_parked();
        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map");
        let (_recorder, events) = Recorder::install(&mut cx, &dm);

        buffer.update(&mut cx, |b, cx| b.edit(2..2, "_edited", cx));
        cx.run_until_parked();

        // Two Changed events fire per buffer edit: DiffMap's own buffer
        // subscription emits a staleness signal (per `DiffMap::new`),
        // then the coordinator's recompute emits another via `set_diff`
        // with the fresh data.
        assert_eq!(
            drain(&events),
            vec![DiffMapEvent::Changed, DiffMapEvent::Changed]
        );
        assert!(!dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn buffer_without_file_path_leaves_diff_map_empty() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, None, "scratch\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer, cx)
        });
        cx.run_until_parked();

        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map allocated even for path-less buffer");
        assert!(dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn track_buffer_is_idempotent_for_same_id() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, None, "x");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer.clone(), cx);
            c.track_buffer(BufferId::new(1), buffer.clone(), cx);
        });
        coordinator.read_with(&cx, |c, _| assert_eq!(c.tracked_count(), 1));
    }

    #[test]
    fn registry_buffer_removed_drops_tracking() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, None, "x");
        let id = BufferId::new(1);

        coordinator.update(&mut cx, |c, cx| c.track_buffer(id, buffer, cx));
        registry.update(&mut cx, |r, cx| {
            r.new_scratch(cx);
            r.remove(id, cx);
        });
        cx.run_until_parked();

        coordinator.read_with(&cx, |c, _| {
            assert_eq!(c.tracked_count(), 0);
            assert!(c.diff_map_for(id).is_none());
        });
    }

    #[test]
    fn unchanged_buffer_text_yields_empty_diff() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/repo").head_file("a.rs", "same\n");
        install_globals(&mut cx, git);
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/repo"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/repo/a.rs")), "same\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer, cx)
        });
        cx.run_until_parked();

        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map");
        assert!(dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn missing_repo_leaves_diff_map_empty() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, Arc::new(FakeGit::new()));
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/no-repo"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/no-repo/a.rs")), "x\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer, cx)
        });
        cx.run_until_parked();

        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map");
        assert!(dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn set_git_root_recomputes_against_new_repo() {
        let mut cx = TestAppContext::single();
        let git = Arc::new(FakeGit::new());
        git.add_repo("/work").head_file("a.rs", "old\n");
        install_globals(&mut cx, git);
        let (_registry, coordinator) = new_coordinator(&mut cx, PathBuf::from("/other"));
        let buffer = new_buffer(&mut cx, Some(PathBuf::from("/work/a.rs")), "new\n");

        coordinator.update(&mut cx, |c, cx| {
            c.track_buffer(BufferId::new(1), buffer.clone(), cx)
        });
        cx.run_until_parked();
        let dm = coordinator
            .read_with(&cx, |c, _| c.diff_map_for(BufferId::new(1)).cloned())
            .expect("diff map");
        assert!(dm.read_with(&cx, |dm, _| dm.diff().is_empty()));

        coordinator.update(&mut cx, |c, cx| c.set_git_root(PathBuf::from("/work"), cx));
        cx.run_until_parked();

        assert!(!dm.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }
}
