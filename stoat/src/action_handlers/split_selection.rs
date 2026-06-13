use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// Active state while the user is typing the split-on-regex pattern
/// into the input modal. Disposed by [`submit`] / [`cancel`].
pub(crate) struct SplitSelectionInputState {
    pub(crate) input: InputView,
    pub(crate) previous_mode: String,
}

pub(super) fn open(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.split_selection_input.is_some() {
        return UpdateEffect::None;
    }
    let previous_mode = stoat.mode.clone();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::SplitSelection, "", "prompt", 1);
    stoat.split_selection_input = Some(SplitSelectionInputState {
        input,
        previous_mode,
    });
    stoat.mode = "prompt".into();
    UpdateEffect::Redraw
}

/// Submit the split-selection regex. Reads the typed pattern,
/// compiles it, and runs `selections.split_each` to split every
/// existing selection at every match. Empty pattern, invalid regex,
/// or no enclosing editor close the input without changing
/// selections. Returns `true` when the input modal was open.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let previous_mode = state.previous_mode.clone();
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
    let Some(editor) = focused_editor_mut(stoat) else {
        return true;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.split_each(buffer_snapshot, |sel| {
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        if start == end {
            return Vec::new();
        }
        let text: String = rope.chunks_in_range(start..end).collect();
        let mut pieces: Vec<Selection<Anchor>> = Vec::new();
        let mut piece_start = start;
        for m in regex.find_iter(&text) {
            let match_start_global = start + m.start();
            let match_end_global = start + m.end();
            pieces.push(make_anchor_selection(
                buffer_snapshot,
                piece_start,
                match_start_global,
            ));
            piece_start = match_end_global;
        }
        if piece_start < end {
            pieces.push(make_anchor_selection(buffer_snapshot, piece_start, end));
        }
        if pieces.is_empty() {
            Vec::new()
        } else {
            pieces
        }
    });
    true
}

/// Cancel the input modal without splitting. Returns `true` when
/// the input modal was open.
pub(crate) fn cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let previous_mode = state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
    true
}

fn make_anchor_selection(
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    start: usize,
    end: usize,
) -> Selection<Anchor> {
    Selection {
        id: 0,
        start: snapshot.anchor_at(start, Bias::Right),
        end: snapshot.anchor_at(end, Bias::Right),
        reversed: false,
        goal: SelectionGoal::None,
    }
}

fn focused_editor_mut(stoat: &mut Stoat) -> Option<&mut crate::editor_state::EditorState> {
    super::focused_editor_mut(stoat)
}
