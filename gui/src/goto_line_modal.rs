use crate::{
    editor::{scroll::autoscroll::AutoscrollStrategy, Editor, EditorEvent},
    modal_layer::ModalView,
    workspace::Workspace,
};
use gpui::{
    div, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity,
    Window,
};
use stoat_action::ActionKind;

/// Modal that takes a line number and previews the destination row on
/// the active editor as the user types. Confirm keeps the previewed
/// cursor; dismiss restores the row that was active when the modal
/// opened.
pub struct GotoLineModal {
    input: Entity<Editor>,
    focus_handle: FocusHandle,
    target: WeakEntity<Editor>,
    original_row: u32,
    _subscriptions: Vec<Subscription>,
}

impl GotoLineModal {
    pub fn new(
        target: WeakEntity<Editor>,
        original_row: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::single_line(window, cx));
        let subscription = cx.subscribe(
            &input,
            |this, _editor, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => this.preview(cx),
            },
        );
        Self {
            input,
            focus_handle: cx.focus_handle(),
            target,
            original_row,
            _subscriptions: vec![subscription],
        }
    }

    #[cfg(test)]
    pub(crate) fn input(&self) -> &Entity<Editor> {
        &self.input
    }

    /// Move the target editor's cursor to the row named by the current
    /// input, scrolling it into view. No-op while the input does not
    /// parse to a line number.
    fn preview(&self, cx: &mut Context<'_, Self>) {
        let Some(row) = parse_target_row(&input_text(&self.input, cx)) else {
            return;
        };
        self.move_target(row, cx);
    }

    fn restore(&self, cx: &mut Context<'_, Self>) {
        self.move_target(self.original_row, cx);
    }

    fn move_target(&self, row: u32, cx: &mut Context<'_, Self>) {
        let Some(editor) = self.target.upgrade() else {
            return;
        };
        editor.update(cx, |ed, cx| {
            ed.set_cursor_at_buffer_row(row, cx);
            ed.request_autoscroll(AutoscrollStrategy::Center, cx);
        });
    }

    fn confirm(&mut self, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }

    fn abort(&mut self, cx: &mut Context<'_, Self>) -> bool {
        self.restore(cx);
        cx.emit(DismissEvent);
        true
    }
}

/// Parse `text` as a 1-based line number and return the 0-based buffer
/// row, or `None` when it is blank or not a number. Line 0 saturates to
/// row 0, matching the count-prefixed `GotoLineNumber`.
fn parse_target_row(text: &str) -> Option<u32> {
    let line: u32 = text.trim().parse().ok()?;
    Some(line.saturating_sub(1))
}

fn input_text(editor: &Entity<Editor>, cx: &App) -> String {
    editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

impl Render for GotoLineModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.input.clone())
    }
}

impl Focusable for GotoLineModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for GotoLineModal {}

impl ModalView for GotoLineModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::DismissModal => self.abort(cx),
            _ => false,
        }
    }

    fn submit_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.confirm(cx)
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.abort(cx)
    }

    fn text_input_editor(&self) -> Option<WeakEntity<Editor>> {
        Some(self.input.downgrade())
    }
}

/// Open the go-to-line modal over the active editor. Constructed in
/// [`Workspace::dispatch_action`] when `OpenGotoLineModal` is
/// dispatched. No-op when no editor is active.
pub fn open_goto_line_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(editor) = workspace.active_editor(cx) else {
        return;
    };
    let original_row = editor.read(cx).primary_cursor_buffer_row(cx);
    let target = editor.downgrade();
    workspace.toggle_modal::<GotoLineModal, _>(window, cx, move |window, cx| {
        GotoLineModal::new(target, original_row, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        globals::ExecutorGlobal, multi_buffer::MultiBuffer,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn new_editor(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn open<'a>(
        cx: &'a mut TestAppContext,
        editor: &Entity<Editor>,
        original_row: u32,
    ) -> (Entity<GotoLineModal>, &'a mut VisualTestContext) {
        let target = editor.downgrade();
        cx.add_window_view(|window, cx| GotoLineModal::new(target, original_row, window, cx))
    }

    fn type_input(modal: &Entity<GotoLineModal>, vcx: &mut VisualTestContext, text: &str) {
        let input = modal.read_with(vcx, |m, _| m.input().clone());
        let buffer = input.read_with(vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line editor has a singleton buffer")
                .clone()
        });
        buffer.update(vcx, |b, cx| b.edit(0..0, text, cx));
        vcx.run_until_parked();
    }

    fn target_row(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> u32 {
        editor.read_with(vcx, |ed, cx| ed.primary_cursor_buffer_row(cx))
    }

    #[test]
    fn parse_target_row_converts_one_based_to_zero_based() {
        assert_eq!(parse_target_row("1"), Some(0));
        assert_eq!(parse_target_row("42"), Some(41));
        assert_eq!(parse_target_row("  7 "), Some(6));
        assert_eq!(parse_target_row("0"), Some(0));
        assert_eq!(parse_target_row(""), None);
        assert_eq!(parse_target_row("abc"), None);
    }

    #[test]
    fn typing_a_line_number_previews_that_row() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let editor = new_editor(&mut cx, "l0\nl1\nl2\nl3\nl4");
        let (modal, vcx) = open(&mut cx, &editor, 0);
        vcx.run_until_parked();

        type_input(&modal, vcx, "3");

        assert_eq!(
            target_row(&editor, vcx),
            2,
            "line 3 (1-based) maps to row 2"
        );
    }

    #[test]
    fn non_numeric_input_leaves_the_cursor_put() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let editor = new_editor(&mut cx, "l0\nl1\nl2");
        let (modal, vcx) = open(&mut cx, &editor, 0);
        vcx.run_until_parked();

        type_input(&modal, vcx, "abc");

        assert_eq!(
            target_row(&editor, vcx),
            0,
            "non-numeric input previews nothing"
        );
    }

    #[test]
    fn confirm_keeps_the_previewed_row() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let editor = new_editor(&mut cx, "l0\nl1\nl2\nl3\nl4");
        let (modal, vcx) = open(&mut cx, &editor, 0);
        vcx.run_until_parked();

        type_input(&modal, vcx, "4");
        vcx.update(|window, cx| {
            modal.update(cx, |m, cx| {
                m.submit_prompt(window, cx);
            });
        });

        assert_eq!(target_row(&editor, vcx), 3, "confirm keeps previewed row 3");
    }

    #[test]
    fn dismiss_restores_the_original_row() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let editor = new_editor(&mut cx, "l0\nl1\nl2\nl3\nl4\nl5");
        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_buffer_row(1, cx));
        let (modal, vcx) = open(&mut cx, &editor, 1);
        vcx.run_until_parked();

        type_input(&modal, vcx, "5");
        assert_eq!(target_row(&editor, vcx), 4, "preview moved to row 4");

        vcx.update(|window, cx| {
            modal.update(cx, |m, cx| {
                m.cancel_prompt(window, cx);
            });
        });

        assert_eq!(
            target_row(&editor, vcx),
            1,
            "dismiss restores the original row"
        );
    }
}
