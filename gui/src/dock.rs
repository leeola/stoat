use crate::{item::ItemHandle, theme::ActiveTheme};
use gpui::{
    div, px, Context, Div, EventEmitter, IntoElement, ParentElement, Render, Styled, Window,
};
use serde::{Deserialize, Serialize};

/// Edge of the window where a dock is pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockSide {
    Left,
    Right,
    Bottom,
}

impl DockSide {
    /// Whether docks pinned to this side lay out as a horizontal
    /// strip -- fixed height, full width. `Bottom` is horizontal;
    /// `Left` and `Right` are vertical (fixed width, full height).
    fn is_horizontal(self) -> bool {
        matches!(self, DockSide::Bottom)
    }
}

/// Whether a dock is rendered, and at what width when open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockVisibility {
    Open { width: u16 },
    Minimized,
    Hidden,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DockEvent {
    ItemChanged,
    VisibilityChanged,
}

/// Single dock entity pinned to one side of the window with one
/// hosted item.
///
/// Multiple docks per side are achieved by holding several
/// `Entity<Dock>` handles at the workspace level. Each instance
/// owns its content via `Box<dyn ItemHandle>`, parallel to the
/// way `Pane` owns its tab list.
pub struct Dock {
    item: Box<dyn ItemHandle>,
    side: DockSide,
    visibility: DockVisibility,
    default_extent: u16,
}

impl EventEmitter<DockEvent> for Dock {}

const MINIMIZED_STRIP_WIDTH_PX: f32 = 6.0;

impl Dock {
    pub fn new(
        item: Box<dyn ItemHandle>,
        side: DockSide,
        default_extent: u16,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.observe_global::<crate::theme::Theme>(|_, cx| cx.notify())
            .detach();
        Self {
            item,
            side,
            visibility: DockVisibility::Open {
                width: default_extent,
            },
            default_extent,
        }
    }

    pub fn item(&self) -> &dyn ItemHandle {
        &*self.item
    }

    pub fn side(&self) -> DockSide {
        self.side
    }

    pub fn visibility(&self) -> DockVisibility {
        self.visibility
    }

    pub fn default_extent(&self) -> u16 {
        self.default_extent
    }

    /// Width the dock occupies on the relevant axis. `Minimized`
    /// reports 1 (a single column), `Hidden` reports 0, `Open`
    /// reports the stored width. Mirrors
    /// `stoat::pane::DockPanel::effective_width`.
    pub fn effective_width(&self) -> u16 {
        match self.visibility {
            DockVisibility::Open { width } => width,
            DockVisibility::Minimized => 1,
            DockVisibility::Hidden => 0,
        }
    }

    pub fn set_item(&mut self, item: Box<dyn ItemHandle>, cx: &mut Context<'_, Self>) {
        self.item = item;
        cx.emit(DockEvent::ItemChanged);
        cx.notify();
    }

    pub fn set_visibility(
        &mut self,
        visibility: DockVisibility,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.visibility == visibility {
            return false;
        }
        self.visibility = visibility;
        cx.emit(DockEvent::VisibilityChanged);
        cx.notify();
        true
    }

    pub fn set_side(&mut self, side: DockSide, cx: &mut Context<'_, Self>) -> bool {
        if self.side == side {
            return false;
        }
        self.side = side;
        cx.emit(DockEvent::ItemChanged);
        cx.notify();
        true
    }

    /// Flip between `Open { default_extent }` and `Minimized`.
    /// Hidden docks resurface at `Open { default_extent }` so the
    /// toggle always lands the dock in a visible state.
    pub fn toggle_minimize(&mut self, cx: &mut Context<'_, Self>) {
        let next = match self.visibility {
            DockVisibility::Open { .. } => DockVisibility::Minimized,
            DockVisibility::Minimized | DockVisibility::Hidden => DockVisibility::Open {
                width: self.default_extent,
            },
        };
        self.set_visibility(next, cx);
    }

    /// Flip between `Open { default_extent }` and `Hidden`. From
    /// `Minimized` the dock opens to `default_extent` rather than
    /// hiding -- show-or-hide treats Minimized as already shown.
    pub fn toggle_open(&mut self, cx: &mut Context<'_, Self>) {
        let next = match self.visibility {
            DockVisibility::Open { .. } => DockVisibility::Hidden,
            DockVisibility::Minimized | DockVisibility::Hidden => DockVisibility::Open {
                width: self.default_extent,
            },
        };
        self.set_visibility(next, cx);
    }

    /// Update the open-state width. Minimized and Hidden states
    /// are unchanged because their effective width does not derive
    /// from the stored value.
    pub fn set_width(&mut self, width: u16, cx: &mut Context<'_, Self>) -> bool {
        match self.visibility {
            DockVisibility::Open { width: current } if current != width => {
                self.visibility = DockVisibility::Open { width };
                cx.emit(DockEvent::VisibilityChanged);
                cx.notify();
                true
            },
            _ => false,
        }
    }
}

impl Render for Dock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let horizontal = self.side.is_horizontal();
        let along_main = |d: Div, extent: f32| {
            if horizontal {
                d.h(px(extent)).w_full()
            } else {
                d.w(px(extent)).h_full()
            }
        };
        match self.visibility {
            DockVisibility::Hidden => along_main(div(), 0.0),
            DockVisibility::Minimized => along_main(
                div()
                    .bg(theme.dock_minimized_background)
                    .border_color(theme.dock_minimized_border),
                MINIMIZED_STRIP_WIDTH_PX,
            ),
            DockVisibility::Open { width } => {
                along_main(div(), f32::from(width)).child(self.item.to_any_view())
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{App, AppContext, Entity, SharedString, TestAppContext};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    struct DockItem {
        label: SharedString,
    }

    impl Render for DockItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for DockItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "DockItem is test-only",
            }
            .fail()
        }
    }

    fn new_dock(cx: &mut TestAppContext, side: DockSide, default_extent: u16) -> Entity<Dock> {
        cx.update(|cx| {
            let item = cx.new(|_| DockItem {
                label: "panel".into(),
            });
            cx.new(|cx| Dock::new(Box::new(item), side, default_extent, cx))
        })
    }

    struct Recorder {
        _subscription: gpui::Subscription,
    }

    fn install_recorder(
        cx: &mut TestAppContext,
        dock: &Entity<Dock>,
    ) -> (Entity<Recorder>, Arc<Mutex<Vec<DockEvent>>>) {
        let events: Arc<Mutex<Vec<DockEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let dock = dock.clone();
        let recorder = cx.update(|cx| {
            let sink = events.clone();
            cx.new(|cx| {
                let subscription = cx.subscribe(&dock, move |_, _, event: &DockEvent, _| {
                    sink.lock().expect("recorder mutex").push(event.clone());
                });
                Recorder {
                    _subscription: subscription,
                }
            })
        });
        (recorder, events)
    }

    fn drain(events: &Arc<Mutex<Vec<DockEvent>>>) -> Vec<DockEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    #[test]
    fn fresh_dock_is_open_at_default_width() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 200);

        dock.read_with(&cx, |d, _| {
            assert_eq!(d.side(), DockSide::Left);
            assert_eq!(d.default_extent(), 200);
            assert_eq!(d.visibility(), DockVisibility::Open { width: 200 });
            assert_eq!(d.effective_width(), 200);
        });
    }

    #[test]
    fn set_visibility_minimized_reports_effective_width_one() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Right, 240);
        let (_r, events) = install_recorder(&mut cx, &dock);

        let changed = dock.update(&mut cx, |d, cx| {
            d.set_visibility(DockVisibility::Minimized, cx)
        });
        cx.run_until_parked();

        assert!(changed);
        assert_eq!(drain(&events), vec![DockEvent::VisibilityChanged]);
        assert_eq!(
            dock.read_with(&cx, |d, _| (d.visibility(), d.effective_width())),
            (DockVisibility::Minimized, 1)
        );
    }

    #[test]
    fn set_visibility_no_op_emits_nothing() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 200);
        let (_r, events) = install_recorder(&mut cx, &dock);

        let changed = dock.update(&mut cx, |d, cx| {
            d.set_visibility(DockVisibility::Open { width: 200 }, cx)
        });
        cx.run_until_parked();

        assert!(!changed);
        assert_eq!(drain(&events), Vec::<DockEvent>::new());
    }

    #[test]
    fn hidden_reports_zero_effective_width() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 200);

        dock.update(&mut cx, |d, cx| {
            d.set_visibility(DockVisibility::Hidden, cx);
        });

        assert_eq!(dock.read_with(&cx, |d, _| d.effective_width()), 0);
    }

    #[test]
    fn toggle_minimize_round_trips_to_default_width() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 180);
        dock.update(&mut cx, |d, cx| d.toggle_minimize(cx));
        assert_eq!(
            dock.read_with(&cx, |d, _| d.visibility()),
            DockVisibility::Minimized
        );

        dock.update(&mut cx, |d, cx| d.toggle_minimize(cx));
        assert_eq!(
            dock.read_with(&cx, |d, _| d.visibility()),
            DockVisibility::Open { width: 180 }
        );
    }

    #[test]
    fn toggle_open_swaps_hidden_and_open() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Right, 160);

        dock.update(&mut cx, |d, cx| d.toggle_open(cx));
        assert_eq!(
            dock.read_with(&cx, |d, _| d.visibility()),
            DockVisibility::Hidden
        );

        dock.update(&mut cx, |d, cx| d.toggle_open(cx));
        assert_eq!(
            dock.read_with(&cx, |d, _| d.visibility()),
            DockVisibility::Open { width: 160 }
        );
    }

    #[test]
    fn toggle_minimize_from_hidden_opens_at_default_width() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 220);

        dock.update(&mut cx, |d, cx| {
            d.set_visibility(DockVisibility::Hidden, cx);
        });
        dock.update(&mut cx, |d, cx| d.toggle_minimize(cx));

        assert_eq!(
            dock.read_with(&cx, |d, _| d.visibility()),
            DockVisibility::Open { width: 220 }
        );
    }

    #[test]
    fn set_width_updates_only_when_open() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 200);
        let (_r, events) = install_recorder(&mut cx, &dock);

        let changed = dock.update(&mut cx, |d, cx| d.set_width(280, cx));
        cx.run_until_parked();
        assert!(changed);
        assert_eq!(drain(&events), vec![DockEvent::VisibilityChanged]);
        assert_eq!(dock.read_with(&cx, |d, _| d.effective_width()), 280);

        let same = dock.update(&mut cx, |d, cx| d.set_width(280, cx));
        cx.run_until_parked();
        assert!(!same);
        assert_eq!(drain(&events), Vec::<DockEvent>::new());

        dock.update(&mut cx, |d, cx| {
            d.set_visibility(DockVisibility::Minimized, cx);
        });
        drain(&events);
        let while_minimized = dock.update(&mut cx, |d, cx| d.set_width(320, cx));
        cx.run_until_parked();
        assert!(!while_minimized);
        assert_eq!(drain(&events), Vec::<DockEvent>::new());
    }

    #[test]
    fn set_item_emits_item_changed() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Left, 200);
        let (_r, events) = install_recorder(&mut cx, &dock);

        let replacement: Box<dyn ItemHandle> = cx.update(|cx| {
            let entity = cx.new(|_| DockItem {
                label: "other".into(),
            });
            Box::new(entity)
        });

        dock.update(&mut cx, |d, cx| d.set_item(replacement, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DockEvent::ItemChanged]);
    }

    #[test]
    fn bottom_is_horizontal_other_sides_vertical() {
        assert!(DockSide::Bottom.is_horizontal());
        assert!(!DockSide::Left.is_horizontal());
        assert!(!DockSide::Right.is_horizontal());
    }

    #[test]
    fn bottom_dock_reports_extent_and_resizes() {
        let mut cx = TestAppContext::single();
        let dock = new_dock(&mut cx, DockSide::Bottom, 240);

        dock.read_with(&cx, |d, _| {
            assert_eq!(d.side(), DockSide::Bottom);
            assert_eq!(d.default_extent(), 240);
            assert_eq!(d.effective_width(), 240);
        });

        let resized = dock.update(&mut cx, |d, cx| d.set_width(180, cx));
        assert!(resized);
        assert_eq!(dock.read_with(&cx, |d, _| d.effective_width()), 180);
    }
}
