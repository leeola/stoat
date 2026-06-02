use crate::{
    buffer::BufferEvent, editor::Editor, item::ItemHandle, status_bar::StatusItemView,
    theme::ActiveTheme, workspace::Workspace,
};
use gpui::{
    div, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
};
use stoat::buffer::Encoding;

/// Status-bar item that surfaces the focused editor's character
/// encoding (UTF-8, Shift-JIS, ...). Rebinds whenever the active pane
/// item changes and subscribes to the editor's singleton buffer so a
/// re-decode is reflected without polling. Clicking the item dispatches
/// the `OpenEncodingPicker` action to switch encodings. Renders nothing
/// when no single-buffer editor is focused.
pub struct EncodingItem {
    workspace: WeakEntity<Workspace>,
    encoding: Option<Encoding>,
    _buffer_subscription: Option<Subscription>,
}

impl EncodingItem {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            encoding: None,
            _buffer_subscription: None,
        }
    }

    fn bind_to_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let buffer = editor
            .read(cx)
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .cloned();

        self.encoding = buffer.as_ref().map(|b| b.read(cx).encoding());
        self._buffer_subscription = buffer.map(|buffer| {
            cx.subscribe(&buffer, |this, buffer, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                    let next = Some(buffer.read(cx).encoding());
                    if this.encoding != next {
                        this.encoding = next;
                        cx.notify();
                    }
                }
            })
        });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.encoding.is_none() && self._buffer_subscription.is_none() {
            return;
        }
        self.encoding = None;
        self._buffer_subscription = None;
        cx.notify();
    }
}

impl Render for EncodingItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = self.encoding.map(|encoding| {
            div()
                .id("encoding-status")
                .px_2()
                .text_color(cx.theme().statusbar_text)
                .child(SharedString::from(encoding.as_str()))
                .on_click(cx.listener(|this, _event, window, cx| {
                    let Some(workspace) = this.workspace.upgrade() else {
                        return;
                    };
                    workspace.update(cx, |ws, cx| {
                        ws.dispatch_action(Box::new(stoat_action::OpenEncodingPicker), window, cx);
                    });
                }))
        });
        div().children(label)
    }
}

impl StatusItemView for EncodingItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<'_, Self>,
    ) {
        let editor = active_pane_item.and_then(|item| item.to_any_view().downcast::<Editor>().ok());
        match editor {
            Some(editor) => self.bind_to_editor(&editor, cx),
            None => self.clear(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::{Editor, EditorMode},
        globals::ExecutorGlobal,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_editor(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), text));
            let multi_buffer = {
                let buffer = buffer.clone();
                cx.new(|cx| MultiBuffer::singleton(buffer, cx))
            };
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let display_map = {
                let buffer = buffer.clone();
                cx.new(|cx| DisplayMap::new(buffer, executor, cx))
            };
            let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn new_item(cx: &mut TestAppContext) -> Entity<EncodingItem> {
        cx.update(|cx| cx.new(|_| EncodingItem::new(WeakEntity::new_invalid())))
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        let item = new_item(&mut cx);
        item.read_with(&cx, |i, _| assert_eq!(i.encoding, None));
    }

    #[test]
    fn binds_to_editor_reports_default_encoding() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_item(&mut cx);
        let editor = new_editor(&mut cx, "abc");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        item.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        item.read_with(&cx, |i, _| assert_eq!(i.encoding, Some(Encoding::Utf8)));
    }

    #[test]
    fn clear_drops_encoding_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_item(&mut cx);
        let editor = new_editor(&mut cx, "abc");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        item.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        item.update(&mut cx, |i, cx| i.set_active_pane_item(None, cx));
        item.read_with(&cx, |i, _| assert_eq!(i.encoding, None));
    }

    #[test]
    fn buffer_encoding_change_updates_item() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_item(&mut cx);
        let editor = new_editor(&mut cx, "abc");
        let buffer = editor
            .read_with(&cx, |ed, cx| {
                ed.multi_buffer().read(cx).as_singleton().cloned()
            })
            .expect("singleton buffer");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        item.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        item.read_with(&cx, |i, _| assert_eq!(i.encoding, Some(Encoding::Utf8)));

        buffer.update(&mut cx, |b, cx| {
            let text = b.text();
            b.set_encoding(Encoding::ShiftJis, &text, cx);
        });
        cx.run_until_parked();
        item.read_with(&cx, |i, _| assert_eq!(i.encoding, Some(Encoding::ShiftJis)));
    }
}
