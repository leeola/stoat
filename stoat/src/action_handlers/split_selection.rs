use crate::{
    action_handlers::search::CursorRegex,
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use stoat_text::{next_char_boundary, Bias, Rope};

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
    let Some(regex) = super::search::compile_cursor_regex(&query) else {
        return true;
    };
    match state.kind {
        RegexSelectKind::Split => split_on_matches(stoat, &regex),
        RegexSelectKind::Select => select_on_matches(stoat, &regex),
    }
    true
}

/// Widen every zero-width piece over one grapheme, so a split lands cursors
/// rather than positions.
///
/// A split names positions as much as spans. Two touching separators, an empty
/// line, and a pattern matching the empty string all pick out somewhere with
/// nothing between. Each is a place the user asked for a cursor, and stoat's
/// cursors cover a cell, so each piece takes the character after it. At the rope
/// end no character follows and the piece stays as it is.
pub(super) fn widen_pieces(rope: &Rope, pieces: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    pieces
        .into_iter()
        .map(|(start, end)| match start == end {
            true => (start, next_char_boundary(rope, start)),
            false => (start, end),
        })
        .collect()
}

/// Split every selection at each match, keeping the gaps between matches.
fn split_on_matches(stoat: &mut Stoat, regex: &CursorRegex) {
    let Some(editor) = focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor
        .selections
        .split_each(buffer_snapshot, Bias::Right, |sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            if start == end {
                return Vec::new();
            }
            let mut pieces: Vec<(usize, usize)> = Vec::new();
            let mut piece_start = start;
            for m in regex.find_iter(rope.regex_slice_input(start..end)) {
                // Two matches touching leave nothing between them, and that gap
                // is still a position the user split at, so it becomes a cursor
                // rather than disappearing.
                pieces.push((piece_start, start + m.start()));
                piece_start = start + m.end();
            }
            if piece_start < end {
                pieces.push((piece_start, end));
            }
            widen_pieces(rope, pieces)
        });
}

/// Replace the selections with every match found inside them. When nothing
/// matches anywhere, the selections are kept and a message is shown.
fn select_on_matches(stoat: &mut Stoat, regex: &CursorRegex) {
    // Collected in one pass rather than asked twice. Whether anything matched
    // is a property of what was collected, so a scan of its own to answer that
    // would be the same work over the same ranges.
    let found: Vec<Vec<(usize, usize)>> = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();

        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let start = buffer_snapshot.resolve_anchor(&sel.start);
                let end = buffer_snapshot.resolve_anchor(&sel.end);
                if start >= end {
                    return Vec::new();
                }

                let matched = regex
                    .find_iter(rope.regex_slice_input(start..end))
                    // A zero-width match on the selection's own end sits just
                    // outside what was searched, which is what an anchor like
                    // `$` produces. Every other empty match names a position
                    // inside it and becomes a cursor there.
                    .filter(|m| start + m.start() != end || m.start() != m.end())
                    .map(|m| (start + m.start(), start + m.end()))
                    .collect();
                widen_pieces(rope, matched)
            })
            .collect()
    };

    if found.iter().all(Vec::is_empty) {
        stoat.set_status("nothing selected");
        return;
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    // The whole set is replaced rather than each selection being split in
    // place, so a selection that matched nothing is gone rather than left
    // sitting where the user searched and found nothing. The guard above
    // returned already if that were every selection, so this is never empty.
    let matches: Vec<(usize, usize)> = found.into_iter().flatten().collect();
    editor
        .selections
        .replace_with_fresh_ids_from_offsets(&matches, Bias::Right, buffer_snapshot);
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

    /// Splitting on adjacent separators leaves a cursor between them.
    ///
    /// The gap between two touching matches is a position the user split at as
    /// much as any other, so it becomes a cursor rather than vanishing. Dropping
    /// it loses a piece the split asked for.
    #[test]
    fn splitting_on_adjacent_matches_leaves_a_cursor_between_them() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("a,,b");
        select_range(&mut h, 0, 4);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text(",");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 1, false), (2, 3, false), (3, 4, false)],
            "the two letters, and a cursor for the gap between the commas",
        );
    }

    /// A pattern matching the empty string selects each position it names.
    ///
    /// Every empty match picks out somewhere inside the selection, and each
    /// becomes a cursor there. The one exception is a match on the selection's
    /// own end, which sits outside what was searched.
    #[test]
    fn selecting_an_empty_matching_pattern_lands_a_cursor_per_position() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc");
        select_range(&mut h, 0, 3);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("o*");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 1, false), (1, 2, false), (2, 3, false)],
            "one cursor per position, the match on the end excluded",
        );
    }

    /// An empty line is a line, so splitting on newlines leaves a cursor on it.
    #[test]
    fn splitting_on_newlines_keeps_a_cursor_on_an_empty_line() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("a\n\nb");
        select_range(&mut h, 0, 4);
        dispatch(&mut h.stoat, &action::SplitSelectionOnNewline);
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 1, false), (2, 3, false), (3, 4, false)],
            "the two letters, and a cursor for the line between them",
        );
    }

    /// Selecting drops the selections that matched nothing, so what is left is
    /// the matches and only the matches.
    ///
    /// Keeping them would leave a cursor sitting where the user searched and
    /// found nothing, which reads as a match that is not one.
    #[test]
    fn selecting_drops_the_selections_that_matched_nothing() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc xyz");
        select_range(&mut h, 0, 3);
        add_range(&mut h, 4, 7);

        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("b");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(1, 2, false)],
            "only the selection that matched survives",
        );
    }

    /// Add a further selection, which mints its own id.
    fn add_range(h: &mut TestHarness, start: usize, end: usize) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor.selections.extend_with_fresh_ids(
            vec![stoat_text::Selection {
                id: 0,
                start: buf_snap.anchor_at(start, stoat_text::Bias::Right),
                end: buf_snap.anchor_at(end, stoat_text::Bias::Right),
                reversed: false,
                goal: stoat_text::SelectionGoal::None,
            }],
            buf_snap,
        );
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

    /// A selection is matched as though it were the whole text, which is what
    /// copying it into a string used to give. Searching it as a range of the
    /// buffer instead would let `^` see the line it was cut from, and it would
    /// not match part-way along one.
    #[test]
    fn an_anchor_matches_at_a_selection_starting_mid_line() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abcdef\n");
        select_range(&mut h, 3, 6);

        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("^d");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(3, 4, false)],
            "the selection's own start counts as the start of the text"
        );
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
