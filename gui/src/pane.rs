use crate::{
    actions::SetActivePane, item::ItemHandle, tab_bar::render_tab_bar, workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, Styled, WeakEntity, Window,
};
use stoat::pane::PaneId;

/// Pane entity holding a tab list of [`ItemHandle`]s plus an active
/// tab index and the pane's own [`FocusHandle`]. Mutations route
/// through the entity so subscribers see [`PaneEvent`]s when items
/// are added, removed, or activated.
///
/// The entity carries `pane_id` -- the [`PaneId`] of its corresponding
/// inner pane in the workspace's [`stoat::pane::PaneTree`] -- and a
/// weak workspace handle so its render's mouse handler can construct
/// and dispatch [`SetActivePane`] without needing closure injection
/// from a parent render.
pub struct Pane {
    pane_id: PaneId,
    workspace: WeakEntity<Workspace>,
    items: Vec<Box<dyn ItemHandle>>,
    active_index: usize,
    focus_handle: FocusHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    ItemAdded { index: usize },
    ItemRemoved { index: usize },
    ActiveItemChanged,
}

impl EventEmitter<PaneEvent> for Pane {}

impl Pane {
    pub fn new(
        pane_id: PaneId,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self {
            pane_id,
            workspace,
            items: Vec::new(),
            active_index: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub fn items(&self) -> &[Box<dyn ItemHandle>] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn active_item(&self) -> Option<&dyn ItemHandle> {
        self.items.get(self.active_index).map(|b| &**b)
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn add_item(&mut self, item: Box<dyn ItemHandle>, cx: &mut Context<'_, Self>) -> usize {
        let index = self.items.len();
        self.items.push(item);
        cx.emit(PaneEvent::ItemAdded { index });
        cx.notify();
        index
    }

    /// Activate the item at `index`. Returns true and emits
    /// [`PaneEvent::ActiveItemChanged`] only when the active index
    /// actually moves; out-of-range indices return false and emit
    /// nothing.
    pub fn activate(&mut self, index: usize, cx: &mut Context<'_, Self>) -> bool {
        if index >= self.items.len() {
            return false;
        }
        if self.active_index == index {
            return false;
        }
        self.active_index = index;
        cx.emit(PaneEvent::ActiveItemChanged);
        cx.notify();
        true
    }

    /// Move the item at `from` to position `to`, shifting other
    /// items between the two indices as needed. Adjusts
    /// `active_index` so it continues to point at the same item
    /// after the move, and emits [`PaneEvent::ActiveItemChanged`]
    /// when the active index shifts (matching the convention used
    /// by [`Pane::remove_item`]). Returns false without side
    /// effects when either index is out of range or when `from`
    /// equals `to`.
    pub fn reorder(&mut self, from: usize, to: usize, cx: &mut Context<'_, Self>) -> bool {
        if from >= self.items.len() || to >= self.items.len() {
            return false;
        }
        if from == to {
            return false;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);

        let old_active = self.active_index;
        let new_active = if old_active == from {
            to
        } else if from < old_active && to >= old_active {
            old_active - 1
        } else if from > old_active && to <= old_active {
            old_active + 1
        } else {
            old_active
        };

        self.active_index = new_active;
        if new_active != old_active {
            cx.emit(PaneEvent::ActiveItemChanged);
        }
        cx.notify();
        true
    }

    /// Remove and return the item at `index`. Clamps the active
    /// index to stay in bounds and emits
    /// [`PaneEvent::ActiveItemChanged`] when the active index
    /// shifts. Out-of-range indices return None without side
    /// effects.
    pub fn remove_item(
        &mut self,
        index: usize,
        cx: &mut Context<'_, Self>,
    ) -> Option<Box<dyn ItemHandle>> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        let active_changed = if self.items.is_empty() {
            self.active_index = 0;
            false
        } else if index < self.active_index {
            self.active_index -= 1;
            true
        } else if self.active_index >= self.items.len() {
            self.active_index = self.items.len() - 1;
            true
        } else {
            false
        };
        cx.emit(PaneEvent::ItemRemoved { index });
        if active_changed {
            cx.emit(PaneEvent::ActiveItemChanged);
        }
        cx.notify();
        Some(removed)
    }
}

impl Render for Pane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let tab_bar = render_tab_bar(self, cx).into_any_element();
        let body: AnyElement = match self.active_item() {
            Some(item) => div().flex_1().child(item.to_any_view()).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child("(scratch)")
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    if let Some(workspace) = this.workspace.upgrade() {
                        let action = SetActivePane {
                            pane_id: this.pane_id.as_ffi(),
                        };
                        workspace.update(cx, |w, cx| {
                            w.dispatch_action(Box::new(action), window, cx);
                        });
                    }
                }),
            )
            .child(tab_bar)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{
        div, point, px, App, AppContext, Entity, IntoElement, Modifiers, Render, SharedString,
        Styled, Subscription, TestAppContext, Window,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use stoat::pane::Axis;

    struct DummyItem {
        label: SharedString,
    }

    impl Render for DummyItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for DummyItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "DummyItem is test-only and does not deserialize",
            }
            .fail()
        }
    }

    fn new_pane(cx: &mut TestAppContext) -> Entity<Pane> {
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("test", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        let weak = workspace.downgrade();
        cx.update(|cx| cx.new(|cx| Pane::new(PaneId::default(), weak, cx)))
    }

    fn new_item(cx: &mut TestAppContext, label: &str) -> Box<dyn ItemHandle> {
        let label = SharedString::from(label.to_string());
        let entity = cx.update(|cx| cx.new(|_| DummyItem { label }));
        Box::new(entity)
    }

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            pane: &Entity<Pane>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<PaneEvent>>>) {
            let events: Arc<Mutex<Vec<PaneEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let pane = pane.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription = cx.subscribe(&pane, move |_, _, event: &PaneEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<PaneEvent>>>) -> Vec<PaneEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    #[test]
    fn fresh_pane_is_empty() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);

        assert!(pane.read_with(&cx, |p, _| p.is_empty()));
        assert_eq!(pane.read_with(&cx, |p, _| p.len()), 0);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 0);
    }

    #[test]
    fn add_item_emits_item_added() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &pane);
        let item = new_item(&mut cx, "alpha");

        let index = pane.update(&mut cx, |p, cx| p.add_item(item, cx));
        cx.run_until_parked();

        assert_eq!(index, 0);
        assert_eq!(drain(&events), vec![PaneEvent::ItemAdded { index: 0 }]);
        assert_eq!(pane.read_with(&cx, |p, _| p.len()), 1);
    }

    #[test]
    fn adding_second_item_does_not_auto_activate() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let first = new_item(&mut cx, "alpha");
        pane.update(&mut cx, |p, cx| p.add_item(first, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &pane);
        let second = new_item(&mut cx, "beta");

        let index = pane.update(&mut cx, |p, cx| p.add_item(second, cx));
        cx.run_until_parked();

        assert_eq!(index, 1);
        assert_eq!(drain(&events), vec![PaneEvent::ItemAdded { index: 1 }]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 0);
    }

    #[test]
    fn activate_emits_when_index_moves() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        let b = new_item(&mut cx, "b");
        pane.update(&mut cx, |p, cx| {
            p.add_item(a, cx);
            p.add_item(b, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.activate(1, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), vec![PaneEvent::ActiveItemChanged]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 1);
    }

    #[test]
    fn activate_same_index_is_a_noop() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        pane.update(&mut cx, |p, cx| p.add_item(a, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.activate(0, cx));
        cx.run_until_parked();

        assert!(!moved);
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
    }

    #[test]
    fn activate_out_of_range_is_refused() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        pane.update(&mut cx, |p, cx| p.add_item(a, cx));
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.activate(7, cx));
        cx.run_until_parked();

        assert!(!moved);
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
    }

    #[test]
    fn remove_active_item_shifts_to_neighbor() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        let b = new_item(&mut cx, "b");
        pane.update(&mut cx, |p, cx| {
            p.add_item(a, cx);
            p.add_item(b, cx);
        });
        // active_index is still 0 (the first item).
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let removed = pane.update(&mut cx, |p, cx| p.remove_item(0, cx));
        cx.run_until_parked();

        assert!(removed.is_some());
        assert_eq!(drain(&events), vec![PaneEvent::ItemRemoved { index: 0 }]);
        // active_index stays 0, now referring to former item b.
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 0);
        assert_eq!(pane.read_with(&cx, |p, _| p.len()), 1);
    }

    #[test]
    fn remove_inactive_below_active_decrements_active_index() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        let b = new_item(&mut cx, "b");
        let c = new_item(&mut cx, "c");
        pane.update(&mut cx, |p, cx| {
            p.add_item(a, cx);
            p.add_item(b, cx);
            p.add_item(c, cx);
            p.activate(2, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let removed = pane.update(&mut cx, |p, cx| p.remove_item(0, cx));
        cx.run_until_parked();

        assert!(removed.is_some());
        assert_eq!(
            drain(&events),
            vec![
                PaneEvent::ItemRemoved { index: 0 },
                PaneEvent::ActiveItemChanged,
            ]
        );
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 1);
    }

    #[test]
    fn remove_tail_when_tail_is_active_clamps_index() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let a = new_item(&mut cx, "a");
        let b = new_item(&mut cx, "b");
        pane.update(&mut cx, |p, cx| {
            p.add_item(a, cx);
            p.add_item(b, cx);
            p.activate(1, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let removed = pane.update(&mut cx, |p, cx| p.remove_item(1, cx));
        cx.run_until_parked();

        assert!(removed.is_some());
        assert_eq!(
            drain(&events),
            vec![
                PaneEvent::ItemRemoved { index: 1 },
                PaneEvent::ActiveItemChanged,
            ]
        );
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 0);
    }

    #[test]
    fn remove_out_of_range_returns_none() {
        let mut cx = TestAppContext::single();
        let pane = new_pane(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let removed = pane.update(&mut cx, |p, cx| p.remove_item(7, cx));
        cx.run_until_parked();

        assert!(removed.is_none());
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
    }

    fn populated_pane(cx: &mut TestAppContext, labels: &[&str]) -> Entity<Pane> {
        let pane = new_pane(cx);
        for label in labels {
            let item = new_item(cx, label);
            pane.update(cx, |p, cx| {
                p.add_item(item, cx);
            });
        }
        pane
    }

    fn item_labels(pane: &Entity<Pane>, cx: &TestAppContext) -> Vec<String> {
        pane.read_with(cx, |p, app| {
            p.items()
                .iter()
                .map(|h| h.tab_label(app).to_string())
                .collect()
        })
    }

    #[test]
    fn reorder_out_of_range_returns_false() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b"]);
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(0, 7, cx));
        cx.run_until_parked();

        assert!(!moved);
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
        assert_eq!(item_labels(&pane, &cx), vec!["a", "b"]);
    }

    #[test]
    fn reorder_same_index_returns_false() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b"]);
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(1, 1, cx));
        cx.run_until_parked();

        assert!(!moved);
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
        assert_eq!(item_labels(&pane, &cx), vec!["a", "b"]);
    }

    #[test]
    fn reorder_active_item_updates_active_index() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b", "c", "d"]);
        pane.update(&mut cx, |p, cx| {
            p.activate(2, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(2, 0, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), vec![PaneEvent::ActiveItemChanged]);
        assert_eq!(item_labels(&pane, &cx), vec!["c", "a", "b", "d"]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 0);
    }

    #[test]
    fn reorder_moving_item_past_active_shifts_active_index_back() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b", "c", "d"]);
        pane.update(&mut cx, |p, cx| {
            p.activate(2, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(0, 3, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), vec![PaneEvent::ActiveItemChanged]);
        assert_eq!(item_labels(&pane, &cx), vec!["b", "c", "d", "a"]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 1);
    }

    #[test]
    fn reorder_moving_item_back_across_active_shifts_active_index_forward() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b", "c", "d"]);
        pane.update(&mut cx, |p, cx| {
            p.activate(2, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(3, 1, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), vec![PaneEvent::ActiveItemChanged]);
        assert_eq!(item_labels(&pane, &cx), vec!["a", "d", "b", "c"]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 3);
    }

    #[test]
    fn reorder_outside_active_range_keeps_active_index() {
        let mut cx = TestAppContext::single();
        let pane = populated_pane(&mut cx, &["a", "b", "c", "d"]);
        pane.update(&mut cx, |p, cx| {
            p.activate(2, cx);
        });
        let (_recorder, events) = Recorder::install(&mut cx, &pane);

        let moved = pane.update(&mut cx, |p, cx| p.reorder(0, 1, cx));
        cx.run_until_parked();

        assert!(moved);
        assert_eq!(drain(&events), Vec::<PaneEvent>::new());
        assert_eq!(item_labels(&pane, &cx), vec!["b", "a", "c", "d"]);
        assert_eq!(pane.read_with(&cx, |p, _| p.active_index()), 2);
    }

    #[test]
    fn mouse_down_dispatches_set_active_pane() {
        let mut cx = TestAppContext::single();
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        let pane_tree = workspace.read_with(&cx, |w, _| w.pane_tree().clone());
        let new_pane_id = pane_tree.update(&mut cx, |t, cx| t.split(Axis::Vertical, cx));
        let original_id = pane_tree.update(&mut cx, |t, cx| {
            t.focus_direction(stoat::pane::Direction::Left, cx);
            t.focus()
        });
        assert_ne!(original_id, new_pane_id);

        let weak = workspace.downgrade();
        let (_pane_view, vcx) =
            cx.add_window_view(|_, cx| Pane::new(new_pane_id, weak.clone(), cx));
        vcx.simulate_click(point(px(10.0), px(10.0)), Modifiers::default());
        vcx.run_until_parked();

        assert_eq!(pane_tree.read_with(vcx, |t, _| t.focus()), new_pane_id);
    }

    fn workspace_weak(cx: &mut TestAppContext) -> WeakEntity<Workspace> {
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        workspace.downgrade()
    }

    #[test]
    fn render_paints_scratch_placeholder_for_empty_pane() {
        let mut cx = TestAppContext::single();
        let weak = workspace_weak(&mut cx);
        let (_pane_view, vcx) = cx.add_window_view(|_, cx| Pane::new(PaneId::default(), weak, cx));
        vcx.run_until_parked();
    }

    #[test]
    fn render_paints_tab_strip_and_active_item_view() {
        let mut cx = TestAppContext::single();
        let weak = workspace_weak(&mut cx);
        let alpha = new_item(&mut cx, "alpha");
        let beta = new_item(&mut cx, "beta");
        let (_pane_view, vcx) = cx.add_window_view(|_, cx| {
            let mut pane = Pane::new(PaneId::default(), weak, cx);
            pane.add_item(alpha, cx);
            pane.add_item(beta, cx);
            pane.activate(1, cx);
            pane
        });
        vcx.run_until_parked();
    }
}
