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
///
/// Snippet items (`is_snippet: true`) parse the insert text via
/// [`crate::completion::snippet::parse`] and install multi-cursor
/// selections at the first tabstop group; remaining groups stash on
/// [`Stoat::active_snippet`] for [`crate::completion::snippet::advance`]
/// to consume on subsequent Tab presses. Plain items insert the text
/// verbatim and collapse the cursor at the inserted end.
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

    let snippet_rendered = if item.is_snippet {
        Some(crate::completion::snippet::parse(&item.insert_text).render())
    } else {
        None
    };
    let inserted_text: &str = snippet_rendered
        .as_ref()
        .map(|r| r.text.as_str())
        .unwrap_or(&item.insert_text);

    {
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(edit_range.clone(), inserted_text);
    }

    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    let active_snippet = if let Some(rendered) = &snippet_rendered {
        let (selections, active) =
            crate::completion::snippet::install(rendered, edit_range.start, new_buf);
        editor.selections.replace_with(selections, new_buf);
        active
    } else {
        let new_offset = edit_range.start + inserted_text.len();
        let anchor = new_buf.anchor_at(new_offset, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
        None
    };

    stoat.pending_completion_request = None;
    crate::completion::request::record_dismiss(stoat);
    stoat.active_snippet = active_snippet;

    UpdateEffect::Redraw
}
