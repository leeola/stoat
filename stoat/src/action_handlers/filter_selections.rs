use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use stoat_text::{Anchor, Selection};

/// Active state while the user is typing the keep- or remove-
/// selections regex into the input modal. The `remove` flag picks
/// between the two operations at submit time.
pub(crate) struct FilterSelectionsInputState {
    pub(crate) input: InputView,
    pub(crate) remove: bool,
    pub(crate) previous_mode: String,
}

pub(super) fn open_keep(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, false)
}

pub(super) fn open_remove(stoat: &mut Stoat) -> UpdateEffect {
    open_with(stoat, true)
}

fn open_with(stoat: &mut Stoat, remove: bool) -> UpdateEffect {
    if stoat.filter_selections_input.is_some() {
        return UpdateEffect::None;
    }
    let previous_mode = stoat.mode.clone();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(
        ws,
        executor,
        SubmitTarget::KeepRemoveSelections,
        "",
        "prompt",
        1,
    );
    stoat.filter_selections_input = Some(FilterSelectionsInputState {
        input,
        remove,
        previous_mode,
    });
    stoat.mode = "prompt".into();
    UpdateEffect::Redraw
}

/// Submit the keep / remove regex. Filters every selection by
/// `regex.is_match(selection_text) XOR remove`. Empty filter result
/// or invalid regex leaves the selections unchanged. Returns `true`
/// when the input modal was open.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.filter_selections_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let previous_mode = state.previous_mode.clone();
    let remove = state.remove;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
    if query.is_empty() {
        return true;
    }
    let regex = match stoat_text::compile_search_regex(&query) {
        Ok(r) => r,
        Err(_) => return true,
    };
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return true;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let kept: Vec<Selection<Anchor>> = editor
        .selections
        .all_anchors()
        .iter()
        .filter(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let text: String = rope.chunks_in_range(start..end).collect();
            regex.is_match(&text) ^ remove
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return true;
    }
    editor.selections.replace_with(kept, buffer_snapshot);
    true
}

/// Cancel the input modal without filtering. Returns `true` when
/// the input modal was open.
pub(crate) fn cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.filter_selections_input.take() else {
        return false;
    };
    let previous_mode = state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
    true
}
