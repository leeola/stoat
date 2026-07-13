use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// Whether the regex modal splits selections at matches or replaces them with
/// the matches. The modal, input state, and submit path are shared. Only the
/// transform applied on submit differs.
#[derive(Copy, Clone)]
pub(crate) enum RegexSelectKind {
    Split,
    Select,
}

/// Active state while the user is typing the regex pattern into the input
/// modal, for either splitting on or selecting matches. Disposed by [`submit`] /
/// [`cancel`].
pub(crate) struct SplitSelectionInputState {
    pub(crate) input: InputView,
    kind: RegexSelectKind,
}

pub(super) fn open(stoat: &mut Stoat, kind: RegexSelectKind) -> UpdateEffect {
    if stoat.split_selection_input.is_some() {
        return UpdateEffect::None;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::SplitSelection, "", "insert", 1);
    stoat.split_selection_input = Some(SplitSelectionInputState { input, kind });
    UpdateEffect::Redraw
}

/// Submit the regex modal. Reads the typed pattern, compiles it, and either
/// splits every selection at each match or replaces the selections with the
/// matches, per the modal's kind. Empty pattern, invalid regex, or no enclosing
/// editor close the input without changing selections. Returns `true` when the
/// input modal was open.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    if query.is_empty() {
        return true;
    }
    let regex = match super::search::compile_search_regex(&query) {
        Ok(r) => r,
        Err(_) => return true,
    };
    match state.kind {
        RegexSelectKind::Split => split_on_matches(stoat, &regex),
        RegexSelectKind::Select => select_on_matches(stoat, &regex),
    }
    true
}

/// Split every selection at each match, keeping the gaps between matches.
fn split_on_matches(stoat: &mut Stoat, regex: &regex::Regex) {
    let Some(editor) = focused_editor_mut(stoat) else {
        return;
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
            pieces.push(make_anchor_selection(
                buffer_snapshot,
                piece_start,
                start + m.start(),
            ));
            piece_start = start + m.end();
        }
        if piece_start < end {
            pieces.push(make_anchor_selection(buffer_snapshot, piece_start, end));
        }
        pieces
    });
}

/// Replace the selections with every match found inside them. When nothing
/// matches anywhere, the selections are kept and a message is shown.
fn select_on_matches(stoat: &mut Stoat, regex: &regex::Regex) {
    let matched = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        editor.selections.all_anchors().iter().any(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            if start >= end {
                return false;
            }
            let text: String = rope.chunks_in_range(start..end).collect();
            regex.find_iter(&text).any(|m| start + m.start() != end)
        })
    };
    if !matched {
        stoat.set_status("nothing selected");
        return;
    }
    let Some(editor) = focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.split_each(buffer_snapshot, |sel| {
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        if start >= end {
            return Vec::new();
        }
        let text: String = rope.chunks_in_range(start..end).collect();
        let mut matches: Vec<Selection<Anchor>> = Vec::new();
        for m in regex.find_iter(&text) {
            let match_start = start + m.start();
            // Skip an empty match sitting at the selection end (from `$`-style
            // anchors), matching Helix.
            if match_start == end {
                continue;
            }
            matches.push(make_anchor_selection(
                buffer_snapshot,
                match_start,
                start + m.end(),
            ));
        }
        matches
    });
}

/// Cancel the input modal without splitting. Returns `true` when
/// the input modal was open.
pub(crate) fn cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
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

    fn select_range(h: &mut TestHarness, start: usize, end: usize) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let start_anchor = buf_snap.anchor_at(start, stoat_text::Bias::Right);
        let end_anchor = buf_snap.anchor_at(end, stoat_text::Bias::Right);
        editor
            .selections
            .transform(buf_snap, |s| stoat_text::Selection {
                id: s.id,
                start: start_anchor,
                end: end_anchor,
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            });
    }

    #[test]
    fn open_creates_input_modal_in_prompt_mode() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        assert_eq!(
            dispatch(&mut h.stoat, &action::SplitSelection),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.split_selection_input.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn submit_splits_selection_on_regex() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 4, false), (7, 11, false)]);
    }

    #[test]
    fn submit_with_no_match_keeps_selection() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc def");
        select_range(&mut h, 0, 7);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 7, false)]);
    }

    #[test]
    fn submit_with_no_match_passes_through() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc");
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 1, false)]);
    }

    #[test]
    fn submit_with_invalid_regex_keeps_selection() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text("[unclosed");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 11, false)]);
    }

    #[test]
    fn empty_submit_keeps_selection() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 11, false)]);
        assert!(h.stoat.split_selection_input.is_none());
    }

    #[test]
    fn escape_cancels_input() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 11, false)]);
        assert!(h.stoat.split_selection_input.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_regex_selects_every_match() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo bar foo");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("foo");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 3, false), (8, 11, false)]);
    }

    #[test]
    fn select_regex_no_match_keeps_selection_and_messages() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc def");
        select_range(&mut h, 0, 7);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("\\d+");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 7, false)]);
        assert_eq!(h.stoat.pending_message.as_deref(), Some("nothing selected"));
    }

    #[test]
    fn select_regex_invalid_regex_keeps_selection() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo bar foo");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("[unclosed");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let spans = editor::selection_spans(&mut h.stoat);
        assert_eq!(spans, vec![(0, 11, false)]);
    }
}
