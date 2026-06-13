use crate::app::{Stoat, UpdateEffect};

/// Arbitrate the Tab key in insert mode. Advance the active snippet
/// placeholder if one is in flight; accept the highlighted completion
/// item if the popup is open; otherwise insert a tab when the cursor
/// follows only whitespace on the current line. Returns
/// [`UpdateEffect::None`] when none of those branches apply so other
/// dispatch layers (or the user) can decide what to do with the
/// keystroke.
pub(super) fn smart_tab(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_snippet.is_some() {
        crate::completion::snippet::advance(stoat);
        return UpdateEffect::Redraw;
    }
    if stoat.pending_completion.is_some() {
        return crate::completion::accept::execute(stoat);
    }
    let Some((editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    if stoat.cursor_after_only_whitespace(editor_id, buffer_id) {
        stoat.editor_insert(editor_id, buffer_id, "\t");
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}

/// Force a completion request even when the buffer signature is
/// unchanged. Clears the dedup signature consulted by
/// [`crate::completion::request::trigger`] so the trigger does not
/// short-circuit, then arms a fresh request.
pub(super) fn trigger_completion(stoat: &mut Stoat) -> UpdateEffect {
    stoat.last_completion_signature = None;
    crate::completion::request::trigger(stoat);
    UpdateEffect::Redraw
}
