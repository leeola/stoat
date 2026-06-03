use crate::{
    editor::{Editor, EditorEvent},
    item::ItemHandle,
    status_bar::StatusItemView,
    theme::ActiveTheme,
};
use gpui::{
    div, App, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window,
};
use stoat_text::cursor_offset;

/// Status-bar item that surfaces the active editor's primary
/// cursor as ` {line}:{col} `, with both indices 1-based per the
/// usual editor convention. Rebinds whenever the active pane
/// item changes; subscribes to the editor's
/// [`EditorEvent::Changed`] so cursor motion refreshes the
/// position without polling.
pub struct CursorPosition {
    position: Option<(u32, u32)>,
    editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
}

impl Default for CursorPosition {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorPosition {
    pub fn new() -> Self {
        Self {
            position: None,
            editor: None,
            _editor_subscription: None,
        }
    }

    pub fn position(&self) -> Option<(u32, u32)> {
        self.position
    }

    fn bind_to_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        self.editor = Some(editor.downgrade());
        self._editor_subscription = Some(cx.subscribe(
            editor,
            |this, editor, _event: &EditorEvent, cx| {
                this.refresh_from_editor(&editor, cx);
            },
        ));
        self.refresh_from_editor(editor, cx);
    }

    fn refresh_from_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let next = compute_position(editor.read(cx), cx);
        if self.position != next {
            self.position = next;
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.position.is_none() && self.editor.is_none() && self._editor_subscription.is_none() {
            return;
        }
        self.position = None;
        self.editor = None;
        self._editor_subscription = None;
        cx.notify();
    }
}

impl Render for CursorPosition {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = self.position.map(|(line, col)| {
            div()
                .px_2()
                .text_color(cx.theme().statusbar_text)
                .child(SharedString::from(format!(" {line}:{col} ")))
        });
        div().children(label)
    }
}

impl StatusItemView for CursorPosition {
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

fn compute_position(editor: &Editor, cx: &App) -> Option<(u32, u32)> {
    let selections = editor.selections().all_anchors();
    let newest = selections.iter().max_by_key(|s| s.id)?;
    let snapshot = editor.multi_buffer().read(cx).snapshot();
    let head = snapshot.resolve_anchor(&newest.head());
    let tail = snapshot.resolve_anchor(&newest.tail());
    let offset = cursor_offset(snapshot.rope(), tail, head);
    let point = snapshot.rope().offset_to_point(offset);
    Some((point.row + 1, point.column + 1))
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

    fn new_cursor_position(cx: &mut TestAppContext) -> Entity<CursorPosition> {
        cx.update(|cx| cx.new(|_| CursorPosition::new()))
    }

    #[test]
    fn new_starts_with_no_position() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_cursor_position(&mut cx);
        item.read_with(&cx, |c, _| assert_eq!(c.position(), None));
    }

    #[test]
    fn binds_to_editor_and_reports_initial_position() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_cursor_position(&mut cx);
        let editor = new_editor(&mut cx, "hello\nworld");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        item.update(&mut cx, |c, cx| {
            c.set_active_pane_item(Some(&*handle), cx);
        });
        item.read_with(&cx, |c, _| assert_eq!(c.position(), Some((1, 1))));
    }

    #[test]
    fn clear_drops_position_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_cursor_position(&mut cx);
        let editor = new_editor(&mut cx, "hi");
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        item.update(&mut cx, |c, cx| {
            c.set_active_pane_item(Some(&*handle), cx);
        });
        item.update(&mut cx, |c, cx| c.set_active_pane_item(None, cx));
        item.read_with(&cx, |c, _| assert_eq!(c.position(), None));
    }

    #[test]
    fn rebinding_swaps_position_source() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_cursor_position(&mut cx);
        let first = new_editor(&mut cx, "a");
        let second = new_editor(&mut cx, "b");
        let first_handle: Box<dyn ItemHandle> = Box::new(first);
        let second_handle: Box<dyn ItemHandle> = Box::new(second);
        item.update(&mut cx, |c, cx| {
            c.set_active_pane_item(Some(&*first_handle), cx);
        });
        item.update(&mut cx, |c, cx| {
            c.set_active_pane_item(Some(&*second_handle), cx);
        });
        item.read_with(&cx, |c, _| assert_eq!(c.position(), Some((1, 1))));
    }

    #[test]
    fn cursor_motion_propagates_through_editor_event() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let item = new_cursor_position(&mut cx);
        let editor = new_editor(&mut cx, "abc\ndef\nghi");
        let handle: Box<dyn ItemHandle> = Box::new(editor.clone());
        item.update(&mut cx, |c, cx| {
            c.set_active_pane_item(Some(&*handle), cx);
        });

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(1, 2, cx));
        cx.run_until_parked();
        item.read_with(&cx, |c, _| assert_eq!(c.position(), Some((2, 3))));
    }
}
