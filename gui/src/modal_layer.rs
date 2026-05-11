use gpui::{
    div, AnyView, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ManagedView, ParentElement, Render, Styled,
    Subscription, Window,
};

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
}

trait ModalViewHandle {
    fn view(&self) -> AnyView;
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |modal, cx| modal.on_before_dismiss(window, cx))
    }
}

struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
    _subscriptions: [Subscription; 1],
}

/// Window-level overlay that owns the currently-open modal. Stoat's
/// keyboard-first flow: a modal is dismissed through its own
/// `DismissEvent` (typically an Escape handler inside the modal),
/// never silently by clicking outside or losing focus.
///
/// The layer holds at most one active modal in this slice; nested
/// modals land under the modal-stack item.
pub struct ModalLayer {
    active_modal: Option<ActiveModal>,
    focus_handle: FocusHandle,
}

/// Emitted by [`ModalLayer`] when a new modal becomes active.
/// Replacing one modal with another fires the event once for the
/// replacement.
pub struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl ModalLayer {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            active_modal: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn has_active_modal(&self) -> bool {
        self.active_modal.is_some()
    }

    /// Return the currently active modal as `Entity<V>` if its type
    /// matches.
    pub fn active_modal<V: 'static>(&self) -> Option<Entity<V>> {
        let active = self.active_modal.as_ref()?;
        active.modal.view().downcast::<V>().ok()
    }

    /// Show `modal` as the active modal, replacing any current one.
    /// Captures the currently-focused element so it can be restored
    /// when the modal closes, and defers focus into the new modal so
    /// the keymap dispatcher sees the modal context on the next
    /// frame. Emits [`ModalOpenedEvent`].
    pub fn show_modal<V>(
        &mut self,
        modal: Entity<V>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) where
        V: ModalView,
    {
        let focus_handle = cx.focus_handle();
        let dismiss_subscription =
            cx.subscribe_in(&modal, window, |this, _, _: &DismissEvent, window, cx| {
                this.hide_modal(window, cx);
            });
        self.active_modal = Some(ActiveModal {
            modal: Box::new(modal.clone()),
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

    /// Hide the active modal if any. Calls `on_before_dismiss` on
    /// the modal; if it returns `false`, the modal stays open and
    /// this returns `false`. Restores focus to the previously
    /// focused element only when the modal currently owns focus, so
    /// programmatic dismiss does not steal focus from elsewhere.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let Some(active) = self.active_modal.as_mut() else {
            return false;
        };

        if !active.modal.on_before_dismiss(window, cx) {
            return false;
        }

        if let Some(active) = self.active_modal.take() {
            if active.focus_handle.contains_focused(window, cx) {
                if let Some(previous) = active.previous_focus_handle {
                    window.focus(&previous);
                }
            }
            cx.notify();
        }
        true
    }

    /// Open a modal of type `V`, or close it if a modal of the same
    /// type is already active (toggle). A different active modal is
    /// replaced. If the current modal vetoes dismissal via
    /// `on_before_dismiss`, the new modal is not shown.
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut Context<'_, Self>, build: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<'_, V>) -> V,
    {
        if let Some(active) = self.active_modal.as_ref() {
            let same_type = active.modal.view().downcast::<V>().is_ok();
            let did_close = self.hide_modal(window, cx);
            if same_type || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build(window, cx));
        self.show_modal(new_modal, window, cx);
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Some(active) = &self.active_modal else {
            return div();
        };
        div()
            .absolute()
            .size_full()
            .inset_0()
            .track_focus(&active.focus_handle)
            .child(active.modal.view())
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

    impl ModalView for OtherModal {}

    fn new_layer(cx: &mut TestAppContext) -> (Entity<ModalLayer>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| ModalLayer::new(cx))
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
}
