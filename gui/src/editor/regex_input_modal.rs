//! Single-line input modal that takes a regex pattern and runs
//! one of three selection-side operations against the active
//! [`Editor`] on confirm:
//!
//! - [`RegexInputKind::Split`] -> [`Editor::split_selection_by_pattern`]
//! - [`RegexInputKind::Keep`] -> filter selections **matching** the regex
//! - [`RegexInputKind::Remove`] -> filter selections **not matching** the regex
//!
//! Mirrors [`crate::lsp::rename::RenameModal`] in shape: a
//! single-line [`Editor`] hosted inside a [`ModalView`] entity,
//! with `PickerConfirm` running the operation and dismissing,
//! and `DismissModal` dismissing without applying.

use crate::{
    editor::{Editor, EditorEvent},
    modal_layer::ModalView,
};
use gpui::{
    div, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Subscription, WeakEntity, Window,
};
use stoat_action::ActionKind;

/// Which selection-side regex operation [`RegexInputModal`]
/// triggers on confirm.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegexInputKind {
    /// Split each selection at every regex match.
    Split,
    /// Keep selections whose covered text matches the regex.
    Keep,
    /// Remove selections whose covered text matches the regex.
    Remove,
}

pub struct RegexInputModal {
    input: Entity<Editor>,
    target_editor: WeakEntity<Editor>,
    kind: RegexInputKind,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl RegexInputModal {
    pub fn new(
        target_editor: WeakEntity<Editor>,
        kind: RegexInputKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::single_line(window, cx));
        let forward_changed = cx.subscribe(&input, |_this, _ed, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                cx.notify();
            }
        });
        Self {
            input,
            target_editor,
            kind,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![forward_changed],
        }
    }

    fn current_text(&self, cx: &gpui::App) -> String {
        let editor = self.input.read(cx);
        editor
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .map(|b| b.read(cx).text())
            .unwrap_or_default()
    }

    fn confirm(&mut self, cx: &mut Context<'_, Self>) {
        let pattern = self.current_text(cx);
        if pattern.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let Some(target) = self.target_editor.upgrade() else {
            cx.emit(DismissEvent);
            return;
        };
        let kind = self.kind;
        target.update(cx, |editor, cx| match kind {
            RegexInputKind::Split => editor.split_selection_by_pattern(&pattern, cx),
            RegexInputKind::Keep => editor.filter_selections_by_pattern(&pattern, false, cx),
            RegexInputKind::Remove => editor.filter_selections_by_pattern(&pattern, true, cx),
        });
        cx.emit(DismissEvent);
    }
}

impl Render for RegexInputModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .child(self.input.clone())
    }
}

impl Focusable for RegexInputModal {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RegexInputModal {}

impl ModalView for RegexInputModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::PickerConfirm => {
                self.confirm(cx);
                true
            },
            ActionKind::DismissModal => {
                cx.emit(DismissEvent);
                true
            },
            _ => false,
        }
    }
}
