//! Single-line input modal that takes a regex pattern and runs
//! one of four operations against the active [`Editor`] on
//! confirm:
//!
//! - [`RegexInputKind::Split`] -> [`Editor::split_selection_by_pattern`]
//! - [`RegexInputKind::Keep`] -> filter selections **matching** the regex
//! - [`RegexInputKind::Remove`] -> filter selections **not matching** the regex
//! - [`RegexInputKind::Search`] -> set the editor's `SearchState` with the chosen direction and
//!   jump the cursor to the first match
//!
//! Mirrors [`crate::lsp::rename::RenameModal`] in shape: a
//! single-line [`Editor`] hosted inside a [`ModalView`] entity,
//! with `PickerConfirm` running the operation and dismissing,
//! and `DismissModal` dismissing without applying.

use crate::{
    editor::{
        search::{SearchDirection, SearchState},
        Editor, EditorEvent,
    },
    modal_layer::ModalView,
    workspace::Workspace,
};
use gpui::{
    div, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Subscription, WeakEntity, Window,
};
use stoat_action::ActionKind;

/// Which operation [`RegexInputModal`] triggers on confirm.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegexInputKind {
    /// Split each selection at every regex match.
    Split,
    /// Keep selections whose covered text matches the regex.
    Keep,
    /// Remove selections whose covered text matches the regex.
    Remove,
    /// Set the active editor's [`SearchState`] with `direction`
    /// and jump the cursor to the first match.
    Search { direction: SearchDirection },
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
        if matches!(kind, RegexInputKind::Search { .. })
            && let Some(prev) = target_editor
                .upgrade()
                .and_then(|ed| ed.read(cx).search_state().map(|s| s.query().to_string()))
            && !prev.is_empty()
        {
            input.update(cx, |ed, cx| ed.apply_text_to_all_cursors(&prev, cx));
        }
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

    #[cfg(test)]
    pub fn input_editor_for_test(&self) -> Entity<Editor> {
        self.input.clone()
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
            RegexInputKind::Search { direction } => {
                editor.set_search_state(Some(SearchState::new(&pattern, direction)), cx);
                editor.search_next(cx);
            },
        });
        cx.emit(DismissEvent);
    }
}

/// Open the search-input modal in `direction` against the active
/// editor. Mirrors `multi_cursor::open_regex_modal` for the
/// Search variant.
pub fn handle_open_search_input(
    workspace: &mut Workspace,
    direction: SearchDirection,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(editor) = workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
    else {
        return;
    };
    let weak_editor = editor.downgrade();
    workspace.modal_layer().update(cx, |layer, cx| {
        layer.toggle_modal(window, cx, |window, cx| {
            RegexInputModal::new(
                weak_editor,
                RegexInputKind::Search { direction },
                window,
                cx,
            )
        });
    });
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

    fn submit_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.confirm(cx);
        true
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }
}
