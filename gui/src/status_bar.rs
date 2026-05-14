pub mod mode_badge;

use crate::item::ItemHandle;
use gpui::{
    div, AnyView, App, Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
};

/// Per-status-item contract. Implementors render their own visual
/// surface and react to the workspace's active-pane-item changes
/// via [`set_active_pane_item`]. The status bar fans the callback
/// out to every registered item on every focus or active-item
/// shift so items can re-bind to the new editor / review / chat /
/// run item without each implementor wiring up its own pane
/// observation.
///
/// Hide / right-click visibility wiring is intentionally absent
/// from the foundation surface; that surface lands with the
/// hideable-item menu work, not here. The callback takes `cx`
/// only -- no `Window` -- because the workspace's broadcast hook
/// runs from a non-window subscription. Items that need window
/// access can fetch it through `cx.window_handle()` on demand.
pub trait StatusItemView: Render + 'static {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<'_, Self>,
    );
}

/// Object-safe wrapper over an `Entity<V: StatusItemView>` so the
/// status bar can hold a heterogeneous mix as `Box<dyn
/// StatusItemViewHandle>`. The blanket impl below makes every
/// `Entity<V: StatusItemView>` usable as a handle.
pub trait StatusItemViewHandle: 'static {
    fn to_any(&self) -> AnyView;
    fn set_active_pane_item(&self, active_pane_item: Option<&dyn ItemHandle>, cx: &mut App);
}

impl<V: StatusItemView> StatusItemViewHandle for Entity<V> {
    fn to_any(&self) -> AnyView {
        AnyView::from(self.clone())
    }

    fn set_active_pane_item(&self, active_pane_item: Option<&dyn ItemHandle>, cx: &mut App) {
        self.update(cx, |item, cx| {
            item.set_active_pane_item(active_pane_item, cx);
        });
    }
}

/// Window-level status strip below the pane tree. Holds two
/// ordered vectors of registered items: `left_items` paint in
/// document order, `right_items` paint right-to-left so the
/// rightmost-registered item lands at the window's right edge.
/// Each item is a heterogeneous `Box<dyn StatusItemViewHandle>`.
///
/// The workspace owns one instance and forwards every active-pane
/// shift through [`set_active_pane_item`], which fans out to all
/// registered items.
pub struct StatusBar {
    left_items: Vec<Box<dyn StatusItemViewHandle>>,
    right_items: Vec<Box<dyn StatusItemViewHandle>>,
}

impl StatusBar {
    pub fn new(_cx: &mut Context<'_, Self>) -> Self {
        Self {
            left_items: Vec::new(),
            right_items: Vec::new(),
        }
    }

    pub fn left_items(&self) -> &[Box<dyn StatusItemViewHandle>] {
        &self.left_items
    }

    pub fn right_items(&self) -> &[Box<dyn StatusItemViewHandle>] {
        &self.right_items
    }

    pub fn add_left_item<V>(&mut self, item: Entity<V>, cx: &mut Context<'_, Self>)
    where
        V: StatusItemView,
    {
        self.left_items.push(Box::new(item));
        cx.notify();
    }

    pub fn add_right_item<V>(&mut self, item: Entity<V>, cx: &mut Context<'_, Self>)
    where
        V: StatusItemView,
    {
        self.right_items.push(Box::new(item));
        cx.notify();
    }

    /// Fan an active-pane-item change out to every registered
    /// item. The workspace calls this after every pane-tree event
    /// so individual items can rebind to the focused buffer /
    /// editor / review / run without each subscribing on its own.
    pub fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<'_, Self>,
    ) {
        for item in self.left_items.iter().chain(self.right_items.iter()) {
            item.set_active_pane_item(active_pane_item, cx);
        }
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let left = div()
            .flex()
            .flex_row()
            .children(self.left_items.iter().map(|item| item.to_any()));
        let right = div()
            .flex()
            .flex_row()
            .children(self.right_items.iter().rev().map(|item| item.to_any()));
        div()
            .flex()
            .flex_row()
            .w_full()
            .justify_between()
            .child(left)
            .child(right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{AppContext, IntoElement, SharedString, TestAppContext};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    struct TestItem {
        label: SharedString,
    }

    impl Render for TestItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for TestItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "TestItem deserialize unused",
            }
            .fail()
        }
    }

    struct Probe {
        observed: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl Probe {
        fn new() -> (Self, Arc<Mutex<Vec<Option<String>>>>) {
            let observed: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    observed: observed.clone(),
                },
                observed,
            )
        }
    }

    impl Render for Probe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl StatusItemView for Probe {
        fn set_active_pane_item(
            &mut self,
            active_pane_item: Option<&dyn ItemHandle>,
            cx: &mut Context<'_, Self>,
        ) {
            let label = active_pane_item.map(|item| item.tab_label(cx).to_string());
            self.observed.lock().expect("probe mutex").push(label);
        }
    }

    fn new_status_bar(cx: &mut TestAppContext) -> Entity<StatusBar> {
        cx.update(|cx| cx.new(StatusBar::new))
    }

    #[test]
    fn new_status_bar_is_empty() {
        let mut cx = TestAppContext::single();
        let bar = new_status_bar(&mut cx);
        bar.read_with(&cx, |bar, _| {
            assert!(bar.left_items().is_empty());
            assert!(bar.right_items().is_empty());
        });
    }

    #[test]
    fn add_left_item_appends_to_left_vec() {
        let mut cx = TestAppContext::single();
        let bar = new_status_bar(&mut cx);
        let (_, vcx) = cx.add_window_view(|_, cx| StatusBar::new(cx));
        let probe = vcx.update(|_, cx| cx.new(|_| Probe::new().0));
        bar.update(vcx, |bar, cx| bar.add_left_item(probe.clone(), cx));
        bar.read_with(vcx, |bar, _| {
            assert_eq!(bar.left_items().len(), 1);
            assert!(bar.right_items().is_empty());
        });
    }

    #[test]
    fn add_right_item_appends_to_right_vec() {
        let mut cx = TestAppContext::single();
        let bar = new_status_bar(&mut cx);
        let (_, vcx) = cx.add_window_view(|_, cx| StatusBar::new(cx));
        let probe = vcx.update(|_, cx| cx.new(|_| Probe::new().0));
        bar.update(vcx, |bar, cx| bar.add_right_item(probe.clone(), cx));
        bar.read_with(vcx, |bar, _| {
            assert!(bar.left_items().is_empty());
            assert_eq!(bar.right_items().len(), 1);
        });
    }

    #[test]
    fn set_active_pane_item_fan_out_reaches_left_and_right_items() {
        let mut cx = TestAppContext::single();
        let (bar, vcx) = cx.add_window_view(|_, cx| StatusBar::new(cx));
        let (left_probe, left_observed) = Probe::new();
        let (right_probe, right_observed) = Probe::new();
        let left = vcx.update(|_, cx| cx.new(|_| left_probe));
        let right = vcx.update(|_, cx| cx.new(|_| right_probe));
        bar.update(vcx, |bar, cx| {
            bar.add_left_item(left, cx);
            bar.add_right_item(right, cx);
        });

        let item: Entity<TestItem> = vcx.update(|_, cx| {
            cx.new(|_| TestItem {
                label: "draft.rs".into(),
            })
        });
        bar.update(vcx, |bar, cx| {
            let handle: Box<dyn ItemHandle> = Box::new(item.clone());
            bar.set_active_pane_item(Some(&*handle), cx);
        });
        vcx.run_until_parked();

        assert_eq!(
            left_observed.lock().expect("left mutex").clone(),
            vec![Some("draft.rs".to_string())],
        );
        assert_eq!(
            right_observed.lock().expect("right mutex").clone(),
            vec![Some("draft.rs".to_string())],
        );
    }

    #[test]
    fn set_active_pane_item_none_forwards_none() {
        let mut cx = TestAppContext::single();
        let (bar, vcx) = cx.add_window_view(|_, cx| StatusBar::new(cx));
        let (probe, observed) = Probe::new();
        let probe_entity = vcx.update(|_, cx| cx.new(|_| probe));
        bar.update(vcx, |bar, cx| bar.add_left_item(probe_entity, cx));

        bar.update(vcx, |bar, cx| {
            bar.set_active_pane_item(None, cx);
        });
        vcx.run_until_parked();

        assert_eq!(observed.lock().expect("mutex").clone(), vec![None]);
    }
}
