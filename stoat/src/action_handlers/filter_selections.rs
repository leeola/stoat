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
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(
        ws,
        executor,
        SubmitTarget::KeepRemoveSelections,
        "",
        "insert",
        1,
    );
    stoat.filter_selections_input = Some(FilterSelectionsInputState { input, remove });
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
    let remove = state.remove;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    if query.is_empty() {
        return true;
    }
    let regex = match super::search::compile_search_regex(&query) {
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
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    true
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        app::UpdateEffect,
        test_harness::{editor, keys, TestHarness},
        Stoat,
    };
    use crossterm::event::{Event, KeyCode};
    use stoat_action as action;

    fn select_two_ranges(h: &mut TestHarness, a: (usize, usize), b: (usize, usize)) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let pieces = vec![
            stoat_text::Selection {
                id: 0,
                start: buf_snap.anchor_at(a.0, stoat_text::Bias::Right),
                end: buf_snap.anchor_at(a.1, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            },
            stoat_text::Selection {
                id: 0,
                start: buf_snap.anchor_at(b.0, stoat_text::Bias::Right),
                end: buf_snap.anchor_at(b.1, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            },
        ];
        editor.selections.split_each(buf_snap, |_| pieces.clone());
    }

    #[test]
    fn keep_filters_to_matching_selections() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(4, 7, false)]);
    }

    #[test]
    fn remove_filters_to_non_matching_selections() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::RemoveSelections);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false)]);
    }

    #[test]
    fn keep_with_no_matches_leaves_selections_unchanged() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (8, 11));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (8, 11, false)]);
    }

    #[test]
    fn remove_with_all_matches_leaves_selections_unchanged() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("123 456");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::RemoveSelections);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (4, 7, false)]);
    }

    #[test]
    fn invalid_regex_is_noop() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.type_text("[unclosed");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (4, 7, false)]);
    }

    #[test]
    fn empty_submit_keeps_selections() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (4, 7, false)]);
        assert!(h.stoat.filter_selections_input.is_none());
    }

    #[test]
    fn escape_cancels_input() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_two_ranges(&mut h, (0, 3), (4, 7));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (4, 7, false)]);
        assert!(h.stoat.filter_selections_input.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn open_creates_input_modal_in_prompt_mode() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc");
        assert_eq!(
            dispatch(&mut h.stoat, &action::KeepSelections),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.filter_selections_input.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }
}
