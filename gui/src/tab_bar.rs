use crate::pane::Pane;
use gpui::{
    div, rgb, AppContext, Context, ElementId, EntityId, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
};

/// Drag payload emitted when a tab is dragged out of its pane.
///
/// `pane` is the [`EntityId`] of the source pane so a drop handler
/// can reject drags that originated in a different pane (cross-pane
/// drag is wired separately).
#[derive(Clone, Copy)]
pub struct DraggedTab {
    pub from_index: usize,
    pub pane: EntityId,
}

const TAB_BG_INACTIVE: u32 = 0x1e1e1e;
const TAB_BG_ACTIVE: u32 = 0x2d2d2d;
const TAB_FG: u32 = 0xcccccc;

/// Build a tab strip for `pane`. Each rendered tab calls back into
/// the pane on click (activate), middle-click (close), and drag-drop
/// (reorder).
///
/// The returned element is meant to be the chrome row above a pane's
/// active item; the caller composes it into the pane's own render.
pub fn render_tab_bar(pane: &Pane, cx: &mut Context<'_, Pane>) -> impl IntoElement {
    let active_index = pane.active_index();
    let pane_id = cx.entity_id();
    let item_count = pane.items().len();

    let mut row = div().flex().flex_row().w_full();
    for ix in 0..item_count {
        let item = &pane.items()[ix];
        let item_id = item.item_id();
        let label = item.tab_label(cx);
        let dirty_marker = if item.is_dirty(cx) { " [+]" } else { "" };
        let display = SharedString::from(format!(" {label}{dirty_marker} "));
        let is_active = ix == active_index;
        let drag_label = display.clone();

        let element_id: ElementId = ("stoat_tab", item_id).into();
        let tab = div()
            .id(element_id)
            .px_2()
            .py_1()
            .text_color(rgb(TAB_FG))
            .bg(rgb(if is_active {
                TAB_BG_ACTIVE
            } else {
                TAB_BG_INACTIVE
            }))
            .child(display)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.activate(ix, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _event, _window, cx| {
                    this.remove_item(ix, cx);
                }),
            )
            .on_drag(
                DraggedTab {
                    from_index: ix,
                    pane: pane_id,
                },
                move |_payload, _offset, _window, cx| {
                    cx.new(|_| DraggedTabView {
                        label: drag_label.clone(),
                    })
                },
            )
            .on_drop::<DraggedTab>(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                if dragged.pane == pane_id {
                    this.reorder(dragged.from_index, ix, cx);
                }
            }));

        row = row.child(tab);
    }
    row
}

struct DraggedTabView {
    label: SharedString,
}

impl Render for DraggedTabView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(rgb(TAB_BG_ACTIVE))
            .text_color(rgb(TAB_FG))
            .child(self.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DeserializeSnafu, ItemError, ItemView};
    use gpui::{App, AppContext, Entity, TestAppContext};
    use serde_json::Value;

    struct TabItem {
        label: SharedString,
        dirty: bool,
    }

    impl Render for TabItem {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full()
        }
    }

    impl ItemView for TabItem {
        fn tab_label(&self, _cx: &App) -> SharedString {
            self.label.clone()
        }

        fn is_dirty(&self, _cx: &App) -> bool {
            self.dirty
        }

        fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
        where
            Self: Sized,
        {
            DeserializeSnafu {
                reason: "TabItem is test-only",
            }
            .fail()
        }
    }

    fn new_pane_with_items(cx: &mut TestAppContext, items: &[(&str, bool)]) -> Entity<Pane> {
        let pane = cx.update(|cx| cx.new(Pane::new));
        for (label, dirty) in items {
            let label = SharedString::from(label.to_string());
            let dirty = *dirty;
            let item = cx.update(|cx| cx.new(|_| TabItem { label, dirty }));
            let handle = Box::new(item);
            pane.update(cx, |p, cx| {
                p.add_item(handle, cx);
            });
        }
        pane
    }

    #[test]
    fn render_does_not_panic_with_mixed_dirty_state() {
        let mut cx = TestAppContext::single();
        let pane = new_pane_with_items(
            &mut cx,
            &[("alpha", false), ("beta", true), ("gamma", false)],
        );
        pane.update(&mut cx, |p, cx| {
            p.activate(1, cx);
        });

        let built = pane.update(&mut cx, |p, cx| {
            let _element = render_tab_bar(p, cx).into_any_element();
            true
        });
        assert!(built);
    }

    #[test]
    fn render_handles_empty_pane() {
        let mut cx = TestAppContext::single();
        let pane = new_pane_with_items(&mut cx, &[]);

        let built = pane.update(&mut cx, |p, cx| {
            let _element = render_tab_bar(p, cx).into_any_element();
            true
        });
        assert!(built);
    }
}
