use crate::{
    buffer::Buffer,
    editor::{Editor, EditorEvent},
    lsp::edit_apply::apply_workspace_edit_to_buffer,
    modal_layer::ModalView,
    workspace::Workspace,
};
use gpui::{
    div, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Subscription, WeakEntity, Window,
};
use lsp_types::{
    Position, RenameParams, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
    WorkDoneProgressParams,
};
use std::sync::Arc;
use stoat::host::{LspServer, OffsetEncoding};
use stoat_action::ActionKind;

/// Single-line input modal that drives an LSP `textDocument/rename`
/// request. The modal is opened by
/// [`Workspace::dispatch_rename_symbol`] after `prepareRename` has
/// resolved the symbol's URI, position, and placeholder text; the
/// embedded [`Editor::single_line`] is seeded with that placeholder.
///
/// Confirm fires `LspServer::rename` and applies the returned
/// [`WorkspaceEdit`] across every open buffer the workspace tracks
/// (multi-file rename) via
/// [`crate::lsp::edit_apply::apply_workspace_edit_to_buffer`].
/// Dismiss emits [`DismissEvent`] so the modal layer tears the modal
/// down without firing the request.
pub struct RenameModal {
    input: Entity<Editor>,
    source_uri: Uri,
    symbol_position: Position,
    encoding: OffsetEncoding,
    server: Arc<dyn LspServer>,
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    source_rope: stoat_text::Rope,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl RenameModal {
    pub fn new(
        placeholder: &str,
        source_uri: Uri,
        symbol_position: Position,
        encoding: OffsetEncoding,
        server: Arc<dyn LspServer>,
        editor: WeakEntity<Editor>,
        workspace: WeakEntity<Workspace>,
        source_rope: stoat_text::Rope,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::single_line(window, cx));
        if !placeholder.is_empty() {
            seed_editor_text(&input, placeholder, cx);
        }
        let forward_changed = cx.subscribe(&input, |_this, _ed, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                cx.notify();
            }
        });
        Self {
            input,
            source_uri,
            symbol_position,
            encoding,
            server,
            editor,
            workspace,
            source_rope,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![forward_changed],
        }
    }

    fn current_text(&self, cx: &gpui::App) -> String {
        current_text(&self.input, cx)
    }

    /// Read the typed text and, when non-empty, spawn a
    /// `textDocument/rename` request that applies the returned edit
    /// across every open buffer. An empty name is a no-op (the modal
    /// still dismisses). The request runs through the gpui local
    /// executor so the modal can dismiss immediately.
    fn confirm(&mut self, cx: &mut Context<'_, Self>) {
        let new_name = self.current_text(cx);
        if new_name.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: self.source_uri.clone(),
                },
                position: self.symbol_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let server = self.server.clone();
        let active_uri = self.source_uri.clone();
        let rope = self.source_rope.clone();
        let encoding = self.encoding;
        let editor = self.editor.clone();
        let workspace = self.workspace.clone();
        let request_id = match workspace.update(cx, |ws, _| ws.bump_lsp_rename_request_id()) {
            Ok(id) => id,
            Err(_) => {
                cx.emit(DismissEvent);
                return;
            },
        };
        cx.spawn(async move |_, cx| {
            let edit = match server.rename(params).await {
                Ok(Some(edit)) => edit,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::rename",
                        ?err,
                        "rename request failed",
                    );
                    return;
                },
            };
            let still_current = workspace
                .update(cx, |ws, _| ws.lsp_rename_request_id() == request_id)
                .unwrap_or(false);
            if !still_current {
                // A newer rename superseded this one; drop the stale
                // edit instead of overwriting whatever the user has
                // typed in the interim.
                return;
            }
            let _ = cx.update(|cx| {
                apply_workspace_edit_to_buffer(
                    &edit,
                    &active_uri,
                    &rope,
                    encoding,
                    &editor,
                    &workspace,
                    cx,
                );
            });
        })
        .detach();
        cx.emit(DismissEvent);
    }
}

fn current_text(input: &Entity<Editor>, cx: &gpui::App) -> String {
    let editor = input.read(cx);
    editor
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

fn seed_editor_text(input: &Entity<Editor>, text: &str, cx: &mut gpui::App) {
    let Some(buffer) = input
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let len = buffer.read(cx).read(|tb| tb.rope().len());
    buffer.update(cx, |b: &mut Buffer, cx| {
        b.edit(0..len, text, cx);
    });
}

impl Render for RenameModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .child(self.input.clone())
    }
}

impl Focusable for RenameModal {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RenameModal {}

impl ModalView for RenameModal {
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

/// Derive the placeholder text seeded into the rename input from a
/// `prepareRename` response. `Range` slices the rope; `RangeWithPlaceholder`
/// carries the placeholder directly; `DefaultBehavior` carries no
/// information about the symbol and yields an empty placeholder.
pub fn placeholder_from_prepare(
    response: lsp_types::PrepareRenameResponse,
    rope: &stoat_text::Rope,
    encoding: OffsetEncoding,
) -> String {
    use lsp_types::PrepareRenameResponse;
    match response {
        PrepareRenameResponse::Range(range) => {
            let start = stoat::lsp::util::lsp_pos_to_byte_offset(rope, range.start, encoding);
            let end = stoat::lsp::util::lsp_pos_to_byte_offset(rope, range.end, encoding);
            rope.slice(start..end).to_string()
        },
        PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => placeholder,
        PrepareRenameResponse::DefaultBehavior { .. } => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{PrepareRenameResponse, Range};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn rng(start: Position, end: Position) -> Range {
        Range { start, end }
    }

    fn rope(text: &str) -> stoat_text::Rope {
        stoat_text::Rope::from(text)
    }

    #[test]
    fn placeholder_range_response_slices_rope() {
        let r = rope("foo bar baz");
        let response = PrepareRenameResponse::Range(rng(pos(0, 4), pos(0, 7)));
        let placeholder = placeholder_from_prepare(response, &r, OffsetEncoding::Utf16);
        assert_eq!(placeholder, "bar");
    }

    #[test]
    fn placeholder_range_with_placeholder_uses_placeholder_text() {
        let r = rope("foo bar baz");
        let response = PrepareRenameResponse::RangeWithPlaceholder {
            range: rng(pos(0, 4), pos(0, 7)),
            placeholder: "renamed".into(),
        };
        let placeholder = placeholder_from_prepare(response, &r, OffsetEncoding::Utf16);
        assert_eq!(placeholder, "renamed");
    }

    #[test]
    fn placeholder_default_behavior_yields_empty_string() {
        let r = rope("foo");
        let response = PrepareRenameResponse::DefaultBehavior {
            default_behavior: true,
        };
        let placeholder = placeholder_from_prepare(response, &r, OffsetEncoding::Utf16);
        assert_eq!(placeholder, "");
    }
}
