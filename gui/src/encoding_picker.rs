//! Encoding picker modal delegate.
//!
//! Lists the supported character encodings. Confirm re-decodes the
//! active buffer's file with the highlighted encoding via
//! [`Workspace::apply_encoding_to_active_buffer`]; dismiss leaves the
//! buffer untouched.

use crate::{
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, IntoElement, ParentElement, SharedString, Styled, Task,
    WeakEntity, Window,
};
use stoat::buffer::Encoding;

pub struct EncodingPickerDelegate {
    workspace: WeakEntity<Workspace>,
    options: Vec<Encoding>,
    selected: usize,
}

impl EncodingPickerDelegate {
    /// Build a delegate that re-decodes the active buffer on confirm,
    /// with the row matching `current` pre-selected.
    pub fn new(workspace: WeakEntity<Workspace>, current: Encoding) -> Self {
        let options = vec![
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
            Encoding::ShiftJis,
            Encoding::Gbk,
        ];
        let selected = options.iter().position(|&o| o == current).unwrap_or(0);
        Self {
            workspace,
            options,
            selected,
        }
    }
}

impl PickerDelegate for EncodingPickerDelegate {
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
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |ws, cx| ws.apply_encoding_to_active_buffer(choice, cx));
            }
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

/// Open the encoding picker for the active editor's singleton buffer.
/// Constructed in [`Workspace::dispatch_action`] when `OpenEncodingPicker`
/// is dispatched. No-op when there is no active single-buffer editor.
pub fn open_encoding_picker(
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
    let current = buffer.read(cx).encoding();
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal::<Picker<EncodingPickerDelegate>, _>(window, cx, move |window, cx| {
        Picker::new(
            EncodingPickerDelegate::new(workspace_handle, current),
            window,
            cx,
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_preselects_the_current_encoding() {
        let delegate = EncodingPickerDelegate::new(WeakEntity::new_invalid(), Encoding::ShiftJis);
        assert_eq!(delegate.selected_index(), 5);
    }

    #[test]
    fn lists_seven_encodings_with_utf8_first() {
        let delegate = EncodingPickerDelegate::new(WeakEntity::new_invalid(), Encoding::Utf8);
        assert_eq!(delegate.match_count(), 7);
        assert_eq!(delegate.selected_index(), 0);
    }
}
