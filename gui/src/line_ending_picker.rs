//! Line-ending picker modal delegate.
//!
//! Lists the three line-ending styles (LF, CRLF, CR). Confirm rewrites
//! every line ending in the active buffer to the highlighted style;
//! dismiss leaves the buffer untouched.

use crate::{
    buffer::Buffer,
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, Entity, IntoElement, ParentElement, SharedString,
    Styled, Task, Window,
};
use stoat::buffer::LineEnding;

pub struct LineEndingPickerDelegate {
    buffer: Entity<Buffer>,
    options: Vec<LineEnding>,
    selected: usize,
}

impl LineEndingPickerDelegate {
    /// Build a delegate that rewrites `buffer` on confirm, with the row
    /// matching `current` pre-selected.
    pub fn new(buffer: Entity<Buffer>, current: LineEnding) -> Self {
        let options = vec![LineEnding::Lf, LineEnding::Crlf, LineEnding::Cr];
        let selected = options.iter().position(|&o| o == current).unwrap_or(0);
        Self {
            buffer,
            options,
            selected,
        }
    }
}

impl PickerDelegate for LineEndingPickerDelegate {
    fn match_count(&self) -> usize {
        self.options.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.options.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, _query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        if let Some(&choice) = self.options.get(self.selected) {
            self.buffer
                .update(cx, |buffer, cx| buffer.set_line_ending(choice, cx));
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some(&option) = self.options.get(ix) else {
            return div().into_any_element();
        };
        let mut row = div()
            .px_2()
            .text_color(cx.theme().modal_picker)
            .child(SharedString::from(option.as_str()));
        if selected {
            row = row.bg(cx.theme().modal_selection);
        }
        row.into_any_element()
    }
}

/// Open the line-ending picker for the active editor's singleton buffer.
/// Constructed in [`Workspace::dispatch_action`] when `OpenLineEndingPicker`
/// is dispatched. No-op when there is no active single-buffer editor.
pub fn open_line_ending_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(editor) = workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|weak| weak.upgrade())
    else {
        return;
    };
    let Some(buffer) = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let current = buffer.read(cx).line_ending();
    workspace.toggle_modal::<Picker<LineEndingPickerDelegate>, _>(window, cx, move |window, cx| {
        Picker::new(LineEndingPickerDelegate::new(buffer, current), window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, TestAppContext};
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

    #[test]
    fn new_preselects_the_current_line_ending() {
        let cx = TestAppContext::single();
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), "a\r\nb")));
        let delegate = LineEndingPickerDelegate::new(buffer, LineEnding::Crlf);
        assert_eq!(delegate.selected_index(), 1);
    }

    #[test]
    fn confirm_rewrites_the_buffer_line_endings() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), "a\nb\nc")));
        let (picker, vcx) = cx.add_window_view({
            let buffer = buffer.clone();
            |window, cx| {
                Picker::new(
                    LineEndingPickerDelegate::new(buffer, LineEnding::Lf),
                    window,
                    cx,
                )
            }
        });

        picker.update(vcx, |p, cx| p.set_selected_index(1, cx));
        vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx);
            });
        });

        let (ending, text) = buffer.read_with(vcx, |b, _| {
            (b.line_ending(), b.read(|tb| tb.rope().to_string()))
        });
        assert_eq!(ending, LineEnding::Crlf);
        assert_eq!(text, "a\r\nb\r\nc");
    }
}
