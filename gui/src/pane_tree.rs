use crate::{pane::Pane, theme::ActiveTheme, workspace::Workspace};
use gpui::{
    div, AnyElement, App, AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement,
    Render, Styled, WeakEntity, Window,
};
use std::collections::BTreeMap;
use stoat::pane::{Axis, Direction, Layout, PaneId, PaneTree as InnerTree};

/// Entity-shaped wrapper around [`stoat::pane::PaneTree`]. Carries the
/// existing split-tree algorithms forward verbatim while exposing only
/// the tree's structural surface to gui consumers. The inner type's
/// `Rect` geometry stays inert because the gpui-backed renderer will
/// compute pane geometry from flex layout rather than reading
/// `Pane::area`.
///
/// `panes` mirrors the inner tree's leaf set: one [`Entity<Pane>`] per
/// [`PaneId`], inserted on construction / [`Self::split`] and pruned on
/// [`Self::close`] / [`Self::close_others`]. The render pass recurses
/// over [`InnerTree::layout`] and looks up each leaf's pane entity here.
pub struct PaneTree {
    inner: InnerTree,
    workspace: WeakEntity<Workspace>,
    panes: BTreeMap<PaneId, Entity<Pane>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneTreeEvent {
    Changed,
}

impl EventEmitter<PaneTreeEvent> for PaneTree {}

impl PaneTree {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<'_, Self>) -> Self {
        let inner = InnerTree::new_default();
        let initial = inner.focus();
        let pane = {
            let workspace = workspace.clone();
            cx.new(|cx| Pane::new(initial, workspace, cx))
        };
        let mut panes = BTreeMap::new();
        panes.insert(initial, pane);
        Self {
            inner,
            workspace,
            panes,
        }
    }

    pub fn focus(&self) -> PaneId {
        self.inner.focus()
    }

    pub fn pane(&self, id: PaneId) -> Option<&Entity<Pane>> {
        self.panes.get(&id)
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
        let pane = {
            let workspace = self.workspace.clone();
            cx.new(|cx| Pane::new(new_id, workspace, cx))
        };
        self.panes.insert(new_id, pane);
        cx.emit(PaneTreeEvent::Changed);
        cx.notify();
        new_id
    }

    pub fn close(&mut self, id: PaneId, cx: &mut Context<'_, Self>) -> bool {
        let closed = self.inner.close(id);
        if closed {
            self.panes.remove(&id);
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

    /// Clone the inner [`stoat::pane::PaneTree`] for workspace
    /// persistence. The inner type already has serde; the wrapper's
    /// parallel `panes: BTreeMap<PaneId, Entity<Pane>>` is rebuilt
    /// on restore via [`Self::apply_state`].
    pub fn inner_clone(&self) -> InnerTree {
        let body = ron::ser::to_string(&self.inner).expect("pane tree round-trips through serde");
        ron::from_str(&body).expect("pane tree round-trips through serde")
    }

    /// Replace the inner [`stoat::pane::PaneTree`] with `inner`,
    /// rebuilding the parallel pane-entity map so every leaf in the
    /// new tree gets a fresh empty [`Pane`]. Callers re-populate pane
    /// content afterward (the persistence layer walks the per-pane
    /// editor snapshots and adds editor items to each).
    pub fn apply_state(&mut self, inner: InnerTree, cx: &mut Context<'_, Self>) {
        let mut panes = BTreeMap::new();
        for id in inner.split_pane_ids() {
            let workspace = self.workspace.clone();
            let pane = cx.new(|cx| Pane::new(id, workspace, cx));
            panes.insert(id, pane);
        }
        self.inner = inner;
        self.panes = panes;
        cx.emit(PaneTreeEvent::Changed);
        cx.notify();
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
                self.panes.remove(&id);
                closed += 1;
            }
        }
        if closed > 0 {
            cx.emit(PaneTreeEvent::Changed);
            cx.notify();
        }
        closed
    }

    fn build_layout_element(&self, layout: &Layout, cx: &App) -> AnyElement {
        match layout {
            Layout::Leaf(pane_id) => {
                let pane = self
                    .panes
                    .get(pane_id)
                    .expect("layout references pane absent from panes map");
                let color = if self.inner.focus() == *pane_id {
                    cx.theme().border_focused
                } else {
                    cx.theme().border_variant
                };
                div()
                    .flex_1()
                    .border_1()
                    .border_color(color)
                    .child(pane.clone())
                    .into_any_element()
            },
            Layout::Split { axis, children } => {
                let base = div().flex().flex_1();
                let mut container = match axis {
                    Axis::Vertical => base.flex_row(),
                    Axis::Horizontal => base.flex_col(),
                };
                for child in children {
                    container = container.child(self.build_layout_element(child, cx));
                }
                container.into_any_element()
            },
        }
    }
}

impl Render for PaneTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let layout = self.inner.layout();
        div()
            .flex()
            .size_full()
            .child(self.build_layout_element(&layout, cx))
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
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("test", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        workspace.read_with(cx, |w, _| w.pane_tree().clone())
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

    fn pane_ids(tree: &Entity<PaneTree>, cx: &TestAppContext) -> Vec<PaneId> {
        tree.read_with(cx, |t, _| t.panes.keys().copied().collect())
    }

    #[test]
    fn panes_map_matches_inner_tree_across_split_and_close() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let initial = tree.read_with(&cx, |t, _| t.focus());

        assert_eq!(pane_ids(&tree, &cx), vec![initial]);

        let new_id = tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        let mut expected = vec![initial, new_id];
        expected.sort();
        let mut actual = pane_ids(&tree, &cx);
        actual.sort();
        assert_eq!(actual, expected);

        tree.update(&mut cx, |t, cx| t.close(new_id, cx));
        assert_eq!(pane_ids(&tree, &cx), vec![initial]);
    }

    #[test]
    fn close_others_prunes_panes_map() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        tree.update(&mut cx, |t, cx| {
            t.split(Axis::Vertical, cx);
            t.split(Axis::Horizontal, cx);
        });
        let focus = tree.read_with(&cx, |t, _| t.focus());

        tree.update(&mut cx, |t, cx| t.close_others(cx));

        assert_eq!(pane_ids(&tree, &cx), vec![focus]);
    }

    #[test]
    fn pane_returns_focused_pane_after_construction() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let focus = tree.read_with(&cx, |t, _| t.focus());

        let pane_id = tree.read_with(&cx, |t, _| {
            t.pane(focus).expect("focused pane registered").entity_id()
        });
        let map_pane_id = tree.read_with(&cx, |t, _| {
            t.panes
                .get(&focus)
                .expect("focused pane in map")
                .entity_id()
        });
        assert_eq!(pane_id, map_pane_id);
    }

    #[test]
    fn pane_returns_none_for_unknown_id() {
        let mut cx = TestAppContext::single();
        let tree = new_tree(&mut cx);
        let new_id = tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        tree.update(&mut cx, |t, cx| {
            t.close(new_id, cx);
        });

        let lookup = tree.read_with(&cx, |t, _| t.pane(new_id).map(|p| p.entity_id()));
        assert_eq!(lookup, None);
    }
}
