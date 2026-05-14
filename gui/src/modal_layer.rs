use crate::workspace::Workspace;
use gpui::{
    div, AnyView, App, AppContext, Context, DismissEvent, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyContext, ManagedView, MouseButton,
    ParentElement, Render, SharedString, Styled, Subscription, WeakEntity, Window,
};
use stoat_action::DismissModal;

/// Modal entity hosted by a [`ModalLayer`]. Builds on gpui's
/// [`ManagedView`] (focus handle + dismiss event + render) and adds a
/// dismiss veto so a modal can refuse to close while it has unsaved
/// state.
///
/// The single hook -- `on_before_dismiss` -- returns `true` to allow
/// dismissal and `false` to keep the modal active. The default
/// implementation returns `true`, so most modals implement this trait
/// by an empty `impl ModalView for Foo {}` block.
pub trait ModalView: ManagedView {
    /// Decide whether the modal may be dismissed right now. Return
    /// `false` to keep the modal in place. The layer respects this
    /// veto on both `hide_modal` and the same-type branch of
    /// `toggle_modal`.
    fn on_before_dismiss(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> bool {
        true
    }

    /// Identifier pushed into the [`ModalLayer`]'s `KeyContext` while
    /// this modal sits on top of the stack. Concrete modal entities
    /// return their type name (`"FileFinder"`, `"CommandPalette"`,
    /// `"DiagnosticsPicker"`, ...) so keymap predicates can target the
    /// active modal. Defaults to `None`, which adds no modal-specific
    /// context.
    fn key_context_name(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    /// Consume a Stoat action routed by
    /// [`ModalLayer::handle_action`]. Returns `true` when the modal
    /// fully handled the action so the caller can short-circuit its
    /// own dispatch. Defaults to `false` so the workspace falls back
    /// to its own match. The picker primitive overrides this to
    /// route select/confirm/dismiss kinds into its delegate.
    fn handle_action(
        &mut self,
        _action: &dyn stoat_action::Action,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> bool {
        false
    }
}

trait ModalViewHandle {
    fn view(&self) -> AnyView;
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> bool;
    fn key_context_name(&self, cx: &App) -> Option<SharedString>;
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut App,
    ) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |modal, cx| modal.on_before_dismiss(window, cx))
    }

    fn key_context_name(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).key_context_name(cx)
    }

    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        self.update(cx, |modal, cx| modal.handle_action(action, window, cx))
    }
}

struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    modal_id: EntityId,
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
    _subscriptions: [Subscription; 1],
}

/// Window-level overlay that owns a stack of modals. The active
/// modal is dismissed through its own `DismissEvent` (typically an
/// Escape handler inside the modal) or by a backdrop click on the
/// layer's overlay. The backdrop click dispatches `DismissModal`
/// through the optional workspace handle; clicks on the modal
/// child itself are bubbled child-first by GPUI and handled there.
///
/// Only the top of the stack renders and receives focus; pushing a
/// new modal stacks it on top of any existing modal so dismissing
/// the new one returns to the previous (e.g., symbol picker opened
/// from inside the file finder). `toggle_modal` operates on the top
/// of the stack -- replace-on-different-type semantics survive
/// unchanged.
///
/// `workspace` is `None` when the layer is constructed in isolation
/// (most internal-state tests); production code wires it via
/// `Workspace::new` so backdrop clicks have a dispatch target.
pub struct ModalLayer {
    workspace: Option<WeakEntity<Workspace>>,
    active_modals: Vec<ActiveModal>,
    focus_handle: FocusHandle,
}

/// Emitted by [`ModalLayer`] when a new modal becomes active.
/// Replacing one modal with another fires the event once for the
/// replacement.
pub struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl ModalLayer {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<'_, Self>) -> Self {
        Self {
            workspace,
            active_modals: Vec::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn has_active_modal(&self) -> bool {
        !self.active_modals.is_empty()
    }

    /// Return the top of the modal stack as `Entity<V>` if its type
    /// matches. Modals deeper in the stack are not visible to type
    /// queries -- only the rendered/focused modal counts as active.
    pub fn active_modal<V: 'static>(&self) -> Option<Entity<V>> {
        let top = self.active_modals.last()?;
        top.modal.view().downcast::<V>().ok()
    }

    /// Push `modal` onto the modal stack. Captures the
    /// currently-focused element so it can be restored when this
    /// modal closes (typically the previous top modal, or the
    /// non-modal focus when this is the first push), and defers
    /// focus into the new modal so the keymap dispatcher sees the
    /// modal context on the next frame. Emits [`ModalOpenedEvent`].
    pub fn show_modal<V>(
        &mut self,
        modal: Entity<V>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) where
        V: ModalView,
    {
        let modal_id = modal.entity_id();
        let focus_handle = cx.focus_handle();
        let dismiss_subscription = cx.subscribe_in(
            &modal,
            window,
            move |this, _, _: &DismissEvent, window, cx| {
                this.dismiss_modal_by_id(modal_id, window, cx);
            },
        );
        self.active_modals.push(ActiveModal {
            modal: Box::new(modal.clone()),
            modal_id,
            previous_focus_handle: window.focused(cx),
            focus_handle,
            _subscriptions: [dismiss_subscription],
        });
        cx.defer_in(window, move |_, window, cx| {
            let handle = Focusable::focus_handle(&modal, cx);
            window.focus(&handle);
        });
        cx.emit(ModalOpenedEvent);
        cx.notify();
    }

    /// Pop the top modal off the stack. Calls `on_before_dismiss` on
    /// the top modal; if it returns `false`, the modal stays open
    /// and this returns `false`. Restores focus to the previously
    /// focused element only when the popped modal currently owns
    /// focus, so programmatic dismiss does not steal focus from
    /// elsewhere.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let len = self.active_modals.len();
        if len == 0 {
            return false;
        }
        if !self.active_modals[len - 1]
            .modal
            .on_before_dismiss(window, cx)
        {
            return false;
        }
        let popped = self.active_modals.pop().expect("checked non-empty");
        if popped.focus_handle.contains_focused(window, cx) {
            if let Some(previous) = popped.previous_focus_handle {
                window.focus(&previous);
            }
        }
        cx.notify();
        true
    }

    fn dismiss_modal_by_id(
        &mut self,
        id: EntityId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(idx) = self.active_modals.iter().position(|m| m.modal_id == id) else {
            return false;
        };
        if !self.active_modals[idx].modal.on_before_dismiss(window, cx) {
            return false;
        }
        let removed = self.active_modals.remove(idx);
        if removed.focus_handle.contains_focused(window, cx) {
            if let Some(previous) = removed.previous_focus_handle {
                window.focus(&previous);
            }
        }
        cx.notify();
        true
    }

    /// Open a modal of type `V`, or close it if a modal of the same
    /// type is currently on top of the stack (toggle). A different
    /// top modal is replaced (popped, then the new modal pushed).
    /// If the current top vetoes dismissal via `on_before_dismiss`,
    /// the new modal is not shown.
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut Context<'_, Self>, build: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<'_, V>) -> V,
    {
        if let Some(top) = self.active_modals.last() {
            let same_type = top.modal.view().downcast::<V>().is_ok();
            let did_close = self.hide_modal(window, cx);
            if same_type || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build(window, cx));
        self.show_modal(new_modal, window, cx);
    }

    /// Forward an action to the top of the modal stack. Returns
    /// `true` when the top modal consumed the action so the caller
    /// (typically [`Workspace::dispatch_action`]) can short-circuit
    /// its own dispatch; returns `false` when no modal is active or
    /// the top modal's [`ModalView::handle_action`] returned `false`.
    pub fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(top) = self.active_modals.last_mut() else {
            return false;
        };
        top.modal.handle_action(action, window, cx)
    }

    /// Compose the `KeyContext` pushed by the layer's wrapping
    /// element. While a modal sits on top of the stack, its
    /// [`ModalView::key_context_name`] is added so keymap predicates
    /// can target the active modal type; with no active modal, or
    /// when the top modal returns `None`, the context is empty.
    pub fn build_key_context(&self, cx: &App) -> KeyContext {
        let mut context = KeyContext::default();
        if let Some(top) = self.active_modals.last() {
            if let Some(name) = top.modal.key_context_name(cx) {
                context.add(name);
            }
        }
        context
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Some(top) = self.active_modals.last() else {
            return div();
        };
        let key_context = self.build_key_context(cx);
        div()
            .absolute()
            .size_full()
            .inset_0()
            .key_context(key_context)
            .track_focus(&top.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    let Some(workspace) = this.workspace.as_ref().and_then(WeakEntity::upgrade)
                    else {
                        return;
                    };
                    // Defer the dispatch so the workspace's `dismiss_modal`
                    // call (which re-enters this layer's `update` to call
                    // `hide_modal`) runs after the listener returns and the
                    // current `update` borrow on the layer is released.
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |w, cx| {
                            w.dispatch_action(Box::new(DismissModal), window, cx);
                        });
                    });
                }),
            )
            .child(top.modal.view())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Subscription, TestAppContext, VisualTestContext};
    use std::sync::{Arc, Mutex};

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

        fn key_context_name(&self, _cx: &App) -> Option<SharedString> {
            Some("TestModal".into())
        }
    }

    struct OtherModal {
        focus_handle: FocusHandle,
    }

    impl OtherModal {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
            }
        }
    }

    impl Render for OtherModal {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl Focusable for OtherModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for OtherModal {}

    impl ModalView for OtherModal {
        fn key_context_name(&self, _cx: &App) -> Option<SharedString> {
            Some("OtherModal".into())
        }
    }

    struct AnonymousModal {
        focus_handle: FocusHandle,
    }

    impl AnonymousModal {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
            }
        }
    }

    impl Render for AnonymousModal {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl Focusable for AnonymousModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for AnonymousModal {}

    impl ModalView for AnonymousModal {}

    fn new_layer(cx: &mut TestAppContext) -> (Entity<ModalLayer>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| ModalLayer::new(None, cx))
    }

    struct Recorder {
        _subscription: Subscription,
    }

    fn record_open_events(
        cx: &mut VisualTestContext,
        layer: &Entity<ModalLayer>,
    ) -> (Entity<Recorder>, Arc<Mutex<usize>>) {
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let layer = layer.clone();
        let recorder = cx.update(|_window, cx| {
            let sink = count.clone();
            cx.new(|cx| {
                let subscription = cx.subscribe(&layer, move |_, _, _: &ModalOpenedEvent, _| {
                    *sink.lock().expect("open count") += 1;
                });
                Recorder {
                    _subscription: subscription,
                }
            })
        });
        (recorder, count)
    }

    #[test]
    fn fresh_layer_has_no_active_modal() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        layer.read_with(vcx, |l, _| {
            assert!(!l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_none());
            let _ = l.focus_handle();
        });
    }

    #[test]
    fn show_modal_sets_active_and_emits_opened() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        let (_r, opens) = record_open_events(vcx, &layer);

        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_some());
            assert!(l.active_modal::<OtherModal>().is_none());
        });
        assert_eq!(*opens.lock().expect("open count"), 1);
    }

    #[test]
    fn dismiss_event_clears_active_modal() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        let modal = layer
            .read_with(vcx, |l, _| l.active_modal::<TestModal>())
            .expect("modal active");
        modal.update(vcx, |_, cx| cx.emit(DismissEvent));
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(!l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_none());
        });
    }

    #[test]
    fn hide_modal_while_empty_is_noop() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        let hidden = layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        assert!(!hidden);
    }

    #[test]
    fn toggle_same_type_closes() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();
        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(!l.has_active_modal());
        });
    }

    #[test]
    fn toggle_swaps_to_different_type() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        let (_r, opens) = record_open_events(vcx, &layer);

        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::new(cx));
        });
        vcx.run_until_parked();
        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<OtherModal, _>(window, cx, |_, cx| OtherModal::new(cx));
        });
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(l.active_modal::<TestModal>().is_none());
            assert!(l.active_modal::<OtherModal>().is_some());
        });
        assert_eq!(*opens.lock().expect("open count"), 2);
    }

    #[test]
    fn on_before_dismiss_veto_keeps_modal() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        layer.update_in(vcx, |l, window, cx| {
            l.toggle_modal::<TestModal, _>(window, cx, |_, cx| TestModal::vetoing(cx));
        });
        vcx.run_until_parked();

        let hidden = layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        assert!(!hidden);
        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_some());
        });
    }

    fn push_modal<V: ModalView>(
        layer: &Entity<ModalLayer>,
        cx: &mut VisualTestContext,
        build: impl FnOnce(&mut Context<'_, V>) -> V,
    ) -> Entity<V> {
        layer.update_in(cx, |l, window, cx| {
            let modal = cx.new(build);
            l.show_modal(modal.clone(), window, cx);
            modal
        })
    }

    #[test]
    fn show_modal_stacks_on_top() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<OtherModal>(&layer, vcx, OtherModal::new);
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<OtherModal>().is_some());
            assert!(l.active_modal::<TestModal>().is_none());
        });
    }

    #[test]
    fn hide_pops_top_and_reveals_lower() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<OtherModal>(&layer, vcx, OtherModal::new);
        vcx.run_until_parked();

        let hidden = layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        assert!(hidden);
        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_some());
            assert!(l.active_modal::<OtherModal>().is_none());
        });
    }

    #[test]
    fn hide_twice_empties_stack() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<OtherModal>(&layer, vcx, OtherModal::new);
        vcx.run_until_parked();

        layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        layer.read_with(vcx, |l, _| {
            assert!(!l.has_active_modal());
        });
    }

    #[test]
    fn dismiss_event_from_lower_modal_removes_that_specific() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        let bottom = push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<OtherModal>(&layer, vcx, OtherModal::new);
        vcx.run_until_parked();

        bottom.update(vcx, |_, cx| cx.emit(DismissEvent));
        vcx.run_until_parked();

        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<OtherModal>().is_some());
            assert!(l.active_modal::<TestModal>().is_none());
        });
    }

    #[test]
    fn veto_on_top_keeps_full_stack() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<TestModal>(&layer, vcx, TestModal::vetoing);
        vcx.run_until_parked();

        let hidden = layer.update_in(vcx, |l, window, cx| l.hide_modal(window, cx));
        assert!(!hidden);
        layer.read_with(vcx, |l, _| {
            assert!(l.has_active_modal());
            assert!(l.active_modal::<TestModal>().is_some());
        });
    }

    #[test]
    fn key_context_empty_when_no_modal() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);

        let context = layer.read_with(vcx, |l, cx| l.build_key_context(cx));
        assert!(!context.contains("TestModal"));
        assert!(!context.contains("OtherModal"));
    }

    #[test]
    fn key_context_includes_top_modal_name() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();

        let context = layer.read_with(vcx, |l, cx| l.build_key_context(cx));
        assert!(context.contains("TestModal"));
        assert!(!context.contains("OtherModal"));
    }

    #[test]
    fn key_context_uses_top_of_stack() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<TestModal>(&layer, vcx, TestModal::new);
        vcx.run_until_parked();
        push_modal::<OtherModal>(&layer, vcx, OtherModal::new);
        vcx.run_until_parked();

        let context = layer.read_with(vcx, |l, cx| l.build_key_context(cx));
        assert!(context.contains("OtherModal"));
        assert!(!context.contains("TestModal"));
    }

    #[test]
    fn key_context_omits_name_when_modal_returns_none() {
        let mut cx = TestAppContext::single();
        let (layer, vcx) = new_layer(&mut cx);
        push_modal::<AnonymousModal>(&layer, vcx, AnonymousModal::new);
        vcx.run_until_parked();

        let context = layer.read_with(vcx, |l, cx| l.build_key_context(cx));
        assert!(!context.contains("TestModal"));
        assert!(!context.contains("OtherModal"));
        assert!(!context.contains("AnonymousModal"));
    }

    /// Bounded modal that leaves backdrop space around it. Plain
    /// `TestModal::render` returns `div().size_full()`, which would
    /// catch every click inside the window and prevent the layer's
    /// backdrop handler from ever firing.
    struct SmallModal {
        focus_handle: FocusHandle,
    }

    impl SmallModal {
        fn new(cx: &mut Context<'_, Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
            }
        }
    }

    impl Render for SmallModal {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            use gpui::px;
            div().w(px(40.0)).h(px(40.0))
        }
    }

    impl Focusable for SmallModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for SmallModal {}

    impl ModalView for SmallModal {}

    /// Adapter view that renders an externally-owned `ModalLayer`
    /// entity. Used by the backdrop-click test below so the test
    /// can drive a layer that was constructed inside a workspace
    /// (and therefore dispatches DismissModal through that
    /// workspace's own layer back into itself).
    struct LayerHost {
        layer: Entity<ModalLayer>,
    }

    impl Render for LayerHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full().child(self.layer.clone())
        }
    }

    #[test]
    fn backdrop_click_dispatches_dismiss_modal() {
        use crate::workspace::Workspace;
        use gpui::{point, px, Modifiers};

        let mut cx = TestAppContext::single();
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        let layer = workspace.read_with(&cx, |w, _| w.modal_layer().clone());
        let layer_for_host = layer.clone();
        let (_host, vcx) = cx.add_window_view(|_, _| LayerHost {
            layer: layer_for_host,
        });
        push_modal::<SmallModal>(&layer, vcx, SmallModal::new);
        vcx.run_until_parked();
        let active_before = layer.read_with(vcx, |l, _| l.active_modal::<SmallModal>());
        assert!(active_before.is_some(), "modal should be open before click");

        // Click far from the SmallModal's 40x40 footprint. GPUI bubbles
        // child-first; the click misses the modal child and lands on the
        // backdrop handler. The handler dispatches `DismissModal` through
        // the workspace, which calls back into this layer's `hide_modal`.
        vcx.simulate_click(point(px(500.0), px(500.0)), Modifiers::default());
        vcx.run_until_parked();

        let active_after = layer.read_with(vcx, |l, _| l.active_modal::<SmallModal>());
        assert!(
            active_after.is_none(),
            "backdrop click should dismiss modal"
        );
    }
}
