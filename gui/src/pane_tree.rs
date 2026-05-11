use gpui::{div, Context, EventEmitter, IntoElement, Render, Styled, Window};
use stoat::pane::{Axis, Direction, PaneId, PaneTree as InnerTree};

/// Entity-shaped wrapper around [`stoat::pane::PaneTree`]. Carries the
/// existing split-tree algorithms forward verbatim while exposing only
/// the tree's structural surface to gui consumers. The inner type's
/// `Rect` geometry stays inert because the gpui-backed renderer will
/// compute pane geometry from flex layout rather than reading
/// `Pane::area`.
pub struct PaneTree {
    inner: InnerTree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneTreeEvent {
    Changed,
}

impl EventEmitter<PaneTreeEvent> for PaneTree {}

impl PaneTree {
    pub fn new() -> Self {
        Self {
            inner: InnerTree::new_default(),
        }
    }

    pub fn focus(&self) -> PaneId {
        self.inner.focus()
    }

    pub fn pane_count(&self) -> usize {
        self.inner.pane_count()
    }

    pub fn split_pane_ids(&self) -> Vec<PaneId> {
        self.inner.split_pane_ids()
    }

    pub fn set_focus(&mut self, id: PaneId, cx: &mut Context<'_, Self>) {
        if self.inner.focus() == id {
            return;
        }
        let before = self.inner.focus();
        self.inner.set_focus(id);
        if self.inner.focus() != before {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
    }

    pub fn split(&mut self, axis: Axis, cx: &mut Context<'_, Self>) -> PaneId {
        let new_id = self.inner.split(axis);
        cx.emit(PaneTreeEvent::Changed);
        cx.notify();
        new_id
    }

    pub fn close(&mut self, id: PaneId, cx: &mut Context<'_, Self>) -> bool {
        let closed = self.inner.close(id);
        if closed {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
        closed
    }

    pub fn focus_direction(&mut self, direction: Direction, cx: &mut Context<'_, Self>) -> bool {
        let moved = self.inner.focus_direction(direction);
        if moved {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
        moved
    }

    pub fn focus_next(&mut self, cx: &mut Context<'_, Self>) {
        let before = self.inner.focus();
        self.inner.focus_next();
        if self.inner.focus() != before {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
    }

    pub fn focus_prev(&mut self, cx: &mut Context<'_, Self>) {
        let before = self.inner.focus();
        self.inner.focus_prev();
        if self.inner.focus() != before {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
    }

    /// Close every split pane except the currently focused one.
    /// Returns the count of panes actually closed; emits a single
    /// `Changed` event when at least one pane was closed.
    pub fn close_others(&mut self, cx: &mut Context<'_, Self>) -> usize {
        let focus = self.inner.focus();
        let to_close: Vec<PaneId> = self
            .inner
            .split_pane_ids()
            .into_iter()
            .filter(|id| *id != focus)
            .collect();
        let mut closed = 0;
        for id in to_close {
            if self.inner.close(id) {
                closed += 1;
            }
        }
        if closed > 0 {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
        closed
    }
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for PaneTree {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // FIXME: replace with the real per-pane render (split-tree
        // subdivision, dividers) once the workspace render
        // composes the pane area. Dispatch lands here from the
        // workspace-hosted input state machine via direct
        // entity.update, not from per-element on_action listeners.
        div().size_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use std::sync::{Arc, Mutex};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            tree: &Entity<PaneTree>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<PaneTreeEvent>>>) {
            let events: Arc<Mutex<Vec<PaneTreeEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let tree = tree.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&tree, move |_, _, event: &PaneTreeEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<PaneTreeEvent>>>) -> Vec<PaneTreeEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn new_tree(cx: &mut TestAppContext) -> Entity<PaneTree> {
        cx.update(|cx| cx.new(|_| PaneTree::new()))
    }

    #[test]
    fn fresh_tree_has_one_pane_with_focus() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);

        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
        let initial_focus = tree.read_with(&cx, |t, _| t.focus());
        let ids = tree.read_with(&cx, |t, _| t.split_pane_ids());
        assert_eq!(ids, vec![initial_focus]);
    }

    #[test]
    fn split_emits_changed_and_moves_focus_to_new_pane() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let initial = tree.read_with(&cx, |t, _| t.focus());
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let new_id = tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 2);
        assert_eq!(tree.read_with(&cx, |t, _| t.focus()), new_id);
        assert_ne!(new_id, initial);
    }

    #[test]
    fn close_after_split_returns_to_single_pane() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let new_id = tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let closed = tree.update(&mut cx, |t, cx| t.close(new_id, cx));
        cx.run_until_parked();

        assert!(closed);
        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn close_last_pane_is_refused_and_does_not_emit() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let only = tree.read_with(&cx, |t, _| t.focus());
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let closed = tree.update(&mut cx, |t, cx| t.close(only, cx));
        cx.run_until_parked();

        assert!(!closed);
        assert_eq!(drain(&events), Vec::<PaneTreeEvent>::new());
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn focus_direction_emits_when_focus_moves() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let moved = tree.update(&mut cx, |t, cx| t.focus_direction(Direction::Left, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
    }

    #[test]
    fn focus_direction_on_single_pane_does_not_emit() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let moved = tree.update(&mut cx, |t, cx| t.focus_direction(Direction::Left, cx));
        cx.run_until_parked();

        assert!(!moved);
        assert_eq!(drain(&events), Vec::<PaneTreeEvent>::new());
    }

    #[test]
    fn set_focus_to_same_pane_is_a_noop() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let focus = tree.read_with(&cx, |t, _| t.focus());
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        tree.update(&mut cx, |t, cx| t.set_focus(focus, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<PaneTreeEvent>::new());
    }

    #[test]
    fn set_focus_to_other_pane_emits() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let initial = tree.read_with(&cx, |t, _| t.focus());
        let other = tree.update(&mut cx, |t, cx| t.split(Axis::Horizontal, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        tree.update(&mut cx, |t, cx| t.set_focus(initial, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
        assert_eq!(tree.read_with(&cx, |t, _| t.focus()), initial);
        assert_ne!(initial, other);
    }

    #[test]
    fn close_others_on_single_pane_is_a_noop() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let closed = tree.update(&mut cx, |t, cx| t.close_others(cx));
        cx.run_until_parked();

        assert_eq!(closed, 0);
        assert_eq!(drain(&events), Vec::<PaneTreeEvent>::new());
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    fn close_others_after_one_split_keeps_focused_pane() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let new_id = tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let closed = tree.update(&mut cx, |t, cx| t.close_others(cx));
        cx.run_until_parked();

        assert_eq!(closed, 1);
        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
        assert_eq!(tree.read_with(&cx, |t, _| t.focus()), new_id);
    }

    #[test]
    fn close_others_after_many_splits_collapses_to_focused() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        tree.update(&mut cx, |t, cx| {
            t.split(Axis::Vertical, cx);
            t.split(Axis::Horizontal, cx);
            t.split(Axis::Vertical, cx);
        });
        let focus = tree.read_with(&cx, |t, _| t.focus());
        let (_recorder, events) = Recorder::install(&mut cx, &tree);

        let closed = tree.update(&mut cx, |t, cx| t.close_others(cx));
        cx.run_until_parked();

        assert_eq!(closed, 3);
        assert_eq!(drain(&events), vec![PaneTreeEvent::Changed]);
        assert_eq!(tree.read_with(&cx, |t, _| t.pane_count()), 1);
        assert_eq!(tree.read_with(&cx, |t, _| t.focus()), focus);
    }
}
