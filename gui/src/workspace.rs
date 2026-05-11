use crate::{
    dock::{Dock, DockSide},
    input_state_machine::InputStateMachine,
    item::ItemHandle,
    modal_layer::{ModalLayer, ModalView},
    pane_tree::PaneTree,
    status_bar::StatusBar,
};
use gpui::{
    div, App, AppContext, Context, Entity, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyContext, Render, SharedString, Styled, Subscription, Window,
};
use std::path::PathBuf;
use stoat::{
    keymap::Keymap,
    pane::{Axis, Direction},
};
use stoat_action::ActionKind;
use stoat_config::Config;

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
    input_state_machine: Entity<InputStateMachine>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
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
        let workspace_handle = cx.weak_entity();
        // FIXME: replace the empty Keymap placeholder with a real
        // compile of the loaded config once the keymap loader
        // (`.todo-plans/TODO.md` "Stoat-native keymap loader") lands.
        let keymap = Keymap::compile(&Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        });
        let input_state_machine = cx.new(|_| InputStateMachine::new(workspace_handle, keymap));
        let keystroke_subscription = cx.observe_keystrokes(|workspace, event, window, cx| {
            let sm = workspace.input_state_machine.clone();
            let keystroke = event.keystroke.clone();
            let actions = sm.update(cx, |sm, cx| sm.feed(&keystroke, cx));
            for action in actions {
                workspace.dispatch_action(action, window, cx);
            }
        });
        Self {
            name: name.into(),
            git_root,
            pane_tree,
            docks: Vec::new(),
            modal_layer,
            status_bar,
            input_state_machine,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![keystroke_subscription],
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

    pub fn input_state_machine(&self) -> &Entity<InputStateMachine> {
        &self.input_state_machine
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

    /// Handle the [`Quit`] action: close the focused pane, then
    /// exit the application when that pane was the last remaining
    /// one. [`PaneTree::close`] returns `false` for the last-pane
    /// case, which is how this distinguishes "closed a pane" from
    /// "refused to close the last pane".
    pub fn handle_quit(&self, cx: &mut Context<'_, Self>) {
        let closed = self.pane_tree.update(cx, |tree, cx| {
            let focus = tree.focus();
            tree.close(focus, cx)
        });
        if !closed {
            cx.quit();
        }
    }

    /// Dispatch a Stoat action resolved by the input state machine.
    /// Routes by [`ActionKind`]: pane-targeted variants update
    /// [`Entity<PaneTree>`], root-targeted variants mutate the
    /// workspace itself. Editor- and modal-targeted variants fall
    /// through to a `tracing::trace` and are wired by the items
    /// that build their target entities (editor render, review
    /// item, modals, etc.).
    pub fn dispatch_action(
        &mut self,
        action: Box<dyn stoat_action::Action>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match action.kind() {
            ActionKind::Quit => self.handle_quit(cx),
            ActionKind::QuitAll => cx.quit(),
            ActionKind::SplitRight => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.split(Axis::Vertical, cx);
                });
            },
            ActionKind::SplitDown => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.split(Axis::Horizontal, cx);
                });
            },
            ActionKind::FocusLeft => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Left, cx);
                });
            },
            ActionKind::FocusRight => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Right, cx);
                });
            },
            ActionKind::FocusUp => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Up, cx);
                });
            },
            ActionKind::FocusDown => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.focus_direction(Direction::Down, cx);
                });
            },
            ActionKind::FocusNext => {
                self.pane_tree.update(cx, |tree, cx| tree.focus_next(cx));
            },
            ActionKind::FocusPrev => {
                self.pane_tree.update(cx, |tree, cx| tree.focus_prev(cx));
            },
            ActionKind::ClosePane => {
                self.pane_tree.update(cx, |tree, cx| {
                    let focus = tree.focus();
                    tree.close(focus, cx);
                });
            },
            ActionKind::CloseOtherPanes => {
                self.pane_tree.update(cx, |tree, cx| {
                    tree.close_others(cx);
                });
            },
            other => {
                tracing::trace!(target: "stoat::dispatch", "unrouted action: {other:?}");
            },
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // FIXME: replace with the real composition (pane area +
        // docks + status bar + modal overlay) once those pieces
        // are rendered. Dispatch routing lives in the workspace-
        // hosted input state machine; the render layer carries
        // focus + key_context only.
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(self.build_key_context())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{
        div, DismissEvent, Focusable, IntoElement, Render, Styled, Subscription, TestAppContext,
        VisualContext, VisualTestContext, Window,
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
    fn fresh_workspace_exposes_input_state_machine_with_defaults() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&cx, |sm, _| {
            assert_eq!(sm.mode(), "normal");
            assert!(!sm.palette_open());
            assert!(!sm.finder_open());
            assert!(!sm.help_open());
            assert!(!sm.claude_focused());
            assert_eq!(sm.pending_count(), None);
        });
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

    #[test]
    fn workspace_handle_quit_closes_focused_pane_when_multiple_exist() {
        use stoat::pane::Axis;
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        pane_tree.update(&mut cx, |t, cx| {
            t.split(Axis::Vertical, cx);
        });
        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 2);

        ws.update(&mut cx, |w, cx| w.handle_quit(cx));
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn workspace_handle_quit_keeps_last_pane() {
        let mut cx = TestAppContext::single();
        let ws = new_workspace(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(&cx, |w, _| w.pane_tree().clone());
        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);

        ws.update(&mut cx, |w, cx| w.handle_quit(cx));
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn workspace_observe_keystrokes_forwards_to_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "5");
        cx.run_until_parked();

        let sm = ws.read_with(&cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), Some(5)));
    }

    fn dispatch<A: stoat_action::Action>(
        ws: &Entity<Workspace>,
        vcx: &mut VisualTestContext,
        action: A,
    ) {
        ws.update_in(vcx, |w, window, cx| {
            w.dispatch_action(Box::new(action), window, cx);
        });
    }

    #[test]
    fn dispatch_split_right_grows_pane_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn dispatch_split_down_grows_pane_tree() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitDown);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);
    }

    #[test]
    fn dispatch_close_pane_after_split_returns_to_single() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::ClosePane);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_close_other_panes_collapses_to_focused() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::SplitDown);
        dispatch(&ws, vcx, stoat_action::SplitRight);
        dispatch(&ws, vcx, stoat_action::defs::pane::CloseOtherPanes);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_focus_direction_changes_focused_pane() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        let after_split = pane_tree.read_with(vcx, |t, _| t.focus());

        dispatch(&ws, vcx, stoat_action::FocusLeft);
        vcx.run_until_parked();

        let after_focus_left = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_ne!(after_focus_left, after_split);
    }

    #[test]
    fn dispatch_focus_next_cycles_through_panes() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());

        dispatch(&ws, vcx, stoat_action::SplitRight);
        let after_split = pane_tree.read_with(vcx, |t, _| t.focus());

        dispatch(&ws, vcx, stoat_action::FocusNext);
        vcx.run_until_parked();
        let after_next = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_ne!(after_next, after_split);

        dispatch(&ws, vcx, stoat_action::FocusNext);
        vcx.run_until_parked();
        let after_wrap = pane_tree.read_with(vcx, |t, _| t.focus());
        assert_eq!(after_wrap, after_split);
    }

    #[test]
    fn dispatch_quit_closes_focused_pane_when_multiple_exist() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        dispatch(&ws, vcx, stoat_action::SplitRight);
        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 2);

        dispatch(&ws, vcx, stoat_action::Quit);
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn dispatch_unknown_action_is_silent() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let before = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));

        dispatch(&ws, vcx, stoat_action::MoveLeft);
        vcx.run_until_parked();

        let after = pane_tree.read_with(vcx, |t, _| (t.pane_count(), t.focus()));
        assert_eq!(before, after);
    }

    #[test]
    fn keystroke_routes_split_right_through_state_machine() {
        let mut cx = TestAppContext::single();
        let (ws, vcx) = new_workspace_in_window(&mut cx, "main", "/tmp/repo");
        let (config, errors) = stoat_config::parse("on key { s -> SplitRight(); }");
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let keymap = Keymap::compile(&config.expect("config"));
        let sm = ws.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_keymap(keymap));

        let pane_tree = ws.read_with(vcx, |w, _| w.pane_tree().clone());
        let window = vcx.window_handle();
        cx.simulate_keystrokes(window, "s");
        cx.run_until_parked();

        assert_eq!(pane_tree.read_with(&cx, |t, _| t.pane_count()), 2);
    }
}
