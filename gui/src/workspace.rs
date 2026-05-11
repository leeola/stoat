use crate::{
    dock::{Dock, DockSide},
    item::ItemHandle,
    modal_layer::{ModalLayer, ModalView},
    pane_tree::PaneTree,
    status_bar::StatusBar,
};
use gpui::{
    div, App, AppContext, Context, Entity, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyContext, Render, SharedString, Styled, Window,
};
use std::path::PathBuf;

/// Top-level workspace entity. Composes the structural pieces of
/// a single Stoat window: the git root, the pane tree, any docks
/// pinned to the window edges, the modal layer overlaid on top of
/// pane content, and the status bar.
///
/// `modal_layer` and `status_bar` are placeholder entities for
/// now; their full implementations land under the corresponding
/// foundation parents in `.todo-plans/TODO.md`.
pub struct Workspace {
    name: SharedString,
    git_root: PathBuf,
    pane_tree: Entity<PaneTree>,
    docks: Vec<Entity<Dock>>,
    modal_layer: Entity<ModalLayer>,
    status_bar: Entity<StatusBar>,
    focus_handle: FocusHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceEvent {
    NameChanged,
    DockAdded { index: usize },
    DockRemoved { index: usize },
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(
        name: impl Into<SharedString>,
        git_root: PathBuf,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let pane_tree = cx.new(|_| PaneTree::new());
        let modal_layer = cx.new(ModalLayer::new);
        let status_bar = cx.new(StatusBar::new);
        Self {
            name: name.into(),
            git_root,
            pane_tree,
            docks: Vec::new(),
            modal_layer,
            status_bar,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn git_root(&self) -> &PathBuf {
        &self.git_root
    }

    pub fn pane_tree(&self) -> &Entity<PaneTree> {
        &self.pane_tree
    }

    pub fn docks(&self) -> &[Entity<Dock>] {
        &self.docks
    }

    pub fn modal_layer(&self) -> &Entity<ModalLayer> {
        &self.modal_layer
    }

    /// Open a modal of type `V` over the workspace, or close it if a
    /// modal of the same type is already active. A different active
    /// modal is replaced. Delegates to [`ModalLayer::toggle_modal`].
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut App, build: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<'_, V>) -> V,
    {
        self.modal_layer
            .update(cx, |layer, cx| layer.toggle_modal(window, cx, build));
    }

    /// Close the currently active modal if any. Returns `false` when
    /// no modal is active or the modal's `on_before_dismiss` vetoes.
    /// Delegates to [`ModalLayer::hide_modal`].
    pub fn dismiss_modal(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.modal_layer
            .update(cx, |layer, cx| layer.hide_modal(window, cx))
    }

    pub fn status_bar(&self) -> &Entity<StatusBar> {
        &self.status_bar
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn set_name(&mut self, name: impl Into<SharedString>, cx: &mut Context<'_, Self>) -> bool {
        let name = name.into();
        if self.name == name {
            return false;
        }
        self.name = name;
        cx.emit(WorkspaceEvent::NameChanged);
        cx.notify();
        true
    }

    pub fn add_dock(
        &mut self,
        item: Box<dyn ItemHandle>,
        side: DockSide,
        default_width: u16,
        cx: &mut Context<'_, Self>,
    ) -> usize {
        let dock = cx.new(|cx| Dock::new(item, side, default_width, cx));
        let index = self.docks.len();
        self.docks.push(dock);
        cx.emit(WorkspaceEvent::DockAdded { index });
        cx.notify();
        index
    }

    /// Compose the `KeyContext` pushed by this workspace's
    /// rendered element. Today only the "Workspace" tag is
    /// added; per-workspace flags (`mode`, `palette_open`,
    /// `finder_open`, `claude_focused`) fold in as their owning
    /// features add the corresponding state to `Workspace`.
    pub fn build_key_context(&self) -> KeyContext {
        let mut context = KeyContext::default();
        context.add("Workspace");
        // FIXME: per-workspace flags land as their owning features
        // (mode, ModalLayer, claude chat) add fields to Workspace.
        context
    }

    /// Remove and return the dock at `index`. Out-of-range indices
    /// return None without side effects.
    pub fn remove_dock(
        &mut self,
        index: usize,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<Dock>> {
        if index >= self.docks.len() {
            return None;
        }
        let removed = self.docks.remove(index);
        cx.emit(WorkspaceEvent::DockRemoved { index });
        cx.notify();
        Some(removed)
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // FIXME: replace with the real composition (pane area +
        // docks + status bar + modal overlay) once those pieces
        // are rendered. The body here is a placeholder; the
        // key_context wiring is the load-bearing part.
        div().size_full().key_context(self.build_key_context())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{
        div, DismissEvent, Focusable, IntoElement, Render, Styled, Subscription, TestAppContext,
        VisualTestContext, Window,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    struct WorkspaceItem {
        label: SharedString,
    }

    impl Render for WorkspaceItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for WorkspaceItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "WorkspaceItem is test-only",
            }
            .fail()
        }
    }

    struct Recorder {
        _subscription: Subscription,
    }

    fn install_recorder(
        cx: &mut TestAppContext,
        ws: &Entity<Workspace>,
    ) -> (Entity<Recorder>, Arc<Mutex<Vec<WorkspaceEvent>>>) {
        let events: Arc<Mutex<Vec<WorkspaceEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let ws = ws.clone();
        let recorder = cx.update(|cx| {
            let sink = events.clone();
            cx.new(|cx| {
                let subscription = cx.subscribe(&ws, move |_, _, event: &WorkspaceEvent, _| {
                    sink.lock().expect("recorder mutex").push(event.clone());
                });
                Recorder {
                    _subscription: subscription,
                }
            })
        });
        (recorder, events)
    }

    fn drain(events: &Arc<Mutex<Vec<WorkspaceEvent>>>) -> Vec<WorkspaceEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn new_workspace(cx: &mut TestAppContext, name: &str, root: &str) -> Entity<Workspace> {
        let name = name.to_string();
        let root = PathBuf::from(root);
        cx.update(|cx| cx.new(|cx| Workspace::new(name, root, cx)))
    }

    fn new_workspace_in_window<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        root: &str,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let name = name.to_string();
        let root = PathBuf::from(root);
        cx.add_window_view(|_window, cx| Workspace::new(name, root, cx))
    }

    struct TestModal {
        focus_handle: FocusHandle,
        veto_dismiss: bool,
    }

    impl TestModal {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                veto_dismiss: false,
            }
        }

        fn vetoing(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                veto_dismiss: true,
            }
        }
    }

    impl Render for TestModal {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl Focusable for TestModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for TestModal {}

    impl ModalView for TestModal {
        fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> bool {
            !self.veto_dismiss
        }
    }

    fn new_item(cx: &mut TestAppContext, label: &str) -> Box<dyn ItemHandle> {
        let label = SharedString::from(label.to_string());
        let entity = cx.update(|cx| cx.new(|_| WorkspaceItem { label }));
        Box::new(entity)
    }

    #[test]
    fn build_key_context_includes_workspace_tag() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        let context = ws.read_with(&cx, |w, _| w.build_key_context());
        assert!(context.contains("Workspace"));
    }

    #[test]
    fn fresh_workspace_exposes_pane_tree_and_no_docks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");

        ws.read_with(&cx, |w, _| {
            assert_eq!(w.name(), &SharedString::from("main"));
            assert_eq!(w.git_root(), &PathBuf::from("/tmp/repo"));
            assert!(w.docks().is_empty());
        });
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        let pane_count = pane_tree.read_with(&cx, |t, _| t.pane_count());
        assert_eq!(pane_count, 1);
    }

    #[test]
    fn set_name_emits_only_on_change() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);

        let same = ws.update(&mut cx, |w, cx| w.set_name("main", cx));
        cx.run_until_parked();
        assert!(!same);
        assert_eq!(drain(&events), Vec::<WorkspaceEvent>::new());

        let changed = ws.update(&mut cx, |w, cx| w.set_name("renamed", cx));
        cx.run_until_parked();
        assert!(changed);
        assert_eq!(drain(&events), vec![WorkspaceEvent::NameChanged]);
        assert_eq!(
            ws.read_with(&cx, |w, _| w.name().clone()),
            SharedString::from("renamed")
        );
    }

    #[test]
    fn add_dock_emits_and_grows_docks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);
        let item = new_item(&mut cx, "outline");

        let index = ws.update(&mut cx, |w, cx| w.add_dock(item, DockSide::Left, 200, cx));
        cx.run_until_parked();

        assert_eq!(index, 0);
        assert_eq!(drain(&events), vec![WorkspaceEvent::DockAdded { index: 0 }]);
        assert_eq!(ws.read_with(&cx, |w, _| w.docks().len()), 1);
    }

    #[test]
    fn remove_dock_out_of_range_returns_none() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let (_r, events) = install_recorder(&mut cx, &ws);

        let removed = ws.update(&mut cx, |w, cx| w.remove_dock(7, cx));
        cx.run_until_parked();

        assert!(removed.is_none());
        assert_eq!(drain(&events), Vec::<WorkspaceEvent>::new());
    }

    #[test]
    fn remove_dock_in_range_emits_and_shrinks() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let item = new_item(&mut cx, "outline");
        ws.update(&mut cx, |w, cx| {
            w.add_dock(item, DockSide::Right, 240, cx);
        });
        let (_r, events) = install_recorder(&mut cx, &ws);

        let removed = ws.update(&mut cx, |w, cx| w.remove_dock(0, cx));
        cx.run_until_parked();

        assert!(removed.is_some());
        assert_eq!(
            drain(&events),
            vec![WorkspaceEvent::DockRemoved { index: 0 }]
        );
        assert_eq!(ws.read_with(&cx, |w, _| w.docks().len()), 0);
    }

    #[test]
    fn workspace_toggle_modal_opens_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_some());
    }

    #[test]
    fn workspace_dismiss_modal_closes_active_modal() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        vcx.run_until_parked();

        assert!(dismissed);
        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_none());
    }

    #[test]
    fn workspace_dismiss_modal_empty_returns_false() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        assert!(!dismissed);
    }

    #[test]
    fn workspace_dismiss_modal_respects_veto() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        ws.update_in(vcx, |w, window, cx| {
            w.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::vetoing(cx));
        });
        vcx.run_until_parked();

        let dismissed = ws.update_in(vcx, |w, window, cx| w.dismiss_modal(window, cx));
        assert!(!dismissed);
        let active = ws.read_with(vcx, |w, cx| {
            w.modal_layer().read(cx).active_modal::<TestModal>()
        });
        assert!(active.is_some());
    }
}
