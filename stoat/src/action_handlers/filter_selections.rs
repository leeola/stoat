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
/// `regex.is_match(selection_text) XOR remove`. A filter that keeps nothing
/// leaves the selections unchanged and reports it. An invalid regex leaves
/// them unchanged in silence. Returns `true` when the input modal was open.
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
    let Some(regex) = super::search::compile_cursor_regex(&query) else {
        stoat.set_status(format!("invalid regex: {query}"));
        return true;
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
            regex.is_match(rope.regex_input(start..end)) ^ remove
        })
        .cloned()
        .collect();
    // Keeping nothing leaves no cursor at all, so the filter is refused. In
    // silence that press is indistinguishable from one that dropped nothing
    // because every selection already matched.
    if kept.is_empty() {
        stoat.set_status("no selections remaining");
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
        let pieces = vec![a, b];
        editor
            .selections
            .split_each(buf_snap, stoat_text::Bias::Right, |_| pieces.clone());
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

    /// An anchor is answered against the buffer, so where a selection sits
    /// decides it, even for two selections holding the same text.
    ///
    /// Matching each as a detached string makes every selection start count as
    /// a line start, which keeps both and leaves the anchor saying nothing.
    #[test]
    fn an_anchor_keeps_only_the_selection_at_a_line_start() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo\nbar foo\n");
        select_two_ranges(&mut h, (0, 3), (8, 11));
        dispatch(&mut h.stoat, &action::KeepSelections);
        h.type_text("^foo");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(editor::selection_spans(&mut h.stoat), vec![(0, 3, false)]);
    }

    /// A filter that keeps nothing is refused, and says so.
    ///
    /// The set has to hold at least one selection, so the refusal is the only
    /// option. Without the message it looks the same as a filter that dropped
    /// nothing because every selection already matched.
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
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no selections remaining"),
        );
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
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no selections remaining"),
        );
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
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("invalid regex: [unclosed"),
            "the pattern that failed is quoted back",
        );
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
