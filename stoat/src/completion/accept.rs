//! Acceptance handler for the completion popup.
//!
//! Replaces the highlighted item's `replace_range` with its
//! `insert_text` in the focused buffer, places the primary cursor at
//! the inserted end, and clears popup state. Bound to `Tab` in
//! insert mode via the arbitration arm in
//! [`crate::app::Stoat::handle_insert_key`].
//!
//! The popup's `replace_range` is in buffer byte offsets captured at
//! trigger time. LSP items widen this beyond the typed prefix when
//! the server returns a `text_edit.range`; non-LSP items scope it to
//! the prefix range. Acceptance reads the range from the chosen
//! item, so both shapes work uniformly.

use crate::{
    app::{Stoat, UpdateEffect},
    pane::{FocusTarget, View},
};
use stoat_text::Bias;

/// Accept the highlighted item in [`Stoat::pending_completion`]. No-op
/// when the popup is not showing, the focused pane is not an editor,
/// or the popup's items list is empty.
pub(crate) fn execute(stoat: &mut Stoat) -> UpdateEffect {
    let Some(popup) = stoat.pending_completion.take() else {
        return UpdateEffect::None;
    };
    let Some(item) = popup.items.into_iter().nth(popup.selected_idx) else {
        return UpdateEffect::None;
    };

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return UpdateEffect::None;
    };
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return UpdateEffect::None;
    };
    let editor = match ws.editors.get_mut(editor_id) {
        Some(e) => e,
        None => return UpdateEffect::None,
    };
    let buffer_id = editor.buffer_id;
    let buffer = match ws.buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };

    let rope_len = buffer.read().expect("poisoned").rope().len();
    let start = item.replace_range.start.min(rope_len);
    let end = item.replace_range.end.min(rope_len);
    let edit_range = start..end;
    {
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(edit_range.clone(), &item.insert_text);
    }

    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let new_offset = edit_range.start + item.insert_text.len();
    let anchor = new_buf.anchor_at(new_offset, Bias::Right);
    editor.selections.transform(new_buf, |s| {
        let mut new = s.clone();
        new.collapse_to(anchor, stoat_text::SelectionGoal::None);
        new
    });

    stoat.pending_completion_request = None;
    crate::completion::request::record_dismiss(stoat);

    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::dispatch,
        completion::{CompletionItem, CompletionPopup, CompletionSource},
        test_harness::TestHarness,
    };
    use std::path::PathBuf;
    use stoat_action::{AcceptCompletion, OpenFile};

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn buffer_text(h: &TestHarness, path: &std::path::Path) -> String {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("buffer registered");
        let buf = ws.buffers.get(id).expect("buffer present");
        let guard = buf.read().expect("buffer lock");
        guard.rope().to_string()
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane(pane_id) = ws.focus else {
            panic!("not a split pane");
        };
        let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
            panic!("not an editor pane");
        };
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snapshot.resolve_anchor(&head)
    }

    fn install_popup(
        h: &mut TestHarness,
        items: Vec<CompletionItem>,
        prefix_range: std::ops::Range<usize>,
    ) {
        h.stoat.pending_completion = Some(CompletionPopup {
            items,
            selected_idx: 0,
            anchor_offset: prefix_range.start,
            prefix_range,
        });
    }

    #[test]
    fn accept_replaces_prefix_with_insert_text() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "foobar".into(),
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
        assert_eq!(cursor_offset(&mut h), 6);
        assert!(h.stoat.pending_completion.is_none());
        assert!(h.stoat.pending_completion_request.is_none());
    }

    #[test]
    fn accept_honors_widened_replace_range_from_lsp_text_edit() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("print");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "println!".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..5,
                insert_text: "println!(\"\")".into(),
            }],
            0..5,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "println!(\"\")");
        assert_eq!(cursor_offset(&mut h), "println!(\"\")".len());
    }

    #[test]
    fn accept_uses_selected_idx_not_first_item() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("fo");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![
                CompletionItem {
                    label: "foo".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..2,
                    insert_text: "foo".into(),
                },
                CompletionItem {
                    label: "foobar".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..2,
                    insert_text: "foobar".into(),
                },
            ],
            selected_idx: 1,
            anchor_offset: 0,
            prefix_range: 0..2,
        });

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
    }

    #[test]
    fn accept_with_no_popup_is_noop() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "abc");
        h.type_keys("a");
        assert!(h.stoat.pending_completion.is_none());

        let effect = dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(buffer_text(&h, &path), "abc");
    }
}
