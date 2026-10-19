use crate::{
    action_handlers::{
        search::{self, CursorRegex},
        view,
    },
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    input_view::{InputView, SubmitTarget},
};
use std::sync::Arc;
use stoat_text::{next_char_boundary, Anchor, Bias, Rope, Selection};

/// Whether the regex modal splits selections at matches or replaces them with
/// the matches. The modal, input state, and submit path are shared. Only the
/// transform differs, which the preview and the submit both apply.
#[derive(Copy, Clone)]
pub(crate) enum RegexSelectKind {
    Split,
    Select,
}

/// Active state while the user types the regex pattern into the input modal,
/// for either splitting on or selecting matches. Disposed by [`submit`] /
/// [`cancel`].
pub(crate) struct SplitSelectionInputState {
    pub(crate) input: InputView,
    pub(crate) kind: RegexSelectKind,
    /// The pane editor's selection set when the prompt opened.
    ///
    /// Each preview starts from this rather than from wherever the last one
    /// landed, so backspace walks a pattern back. The preview for `fo` is the
    /// same whether the user typed up to it or deleted down to it.
    origin_selections: Arc<[Selection<Anchor>]>,
    /// The pane editor's scroll position when the prompt opened, as
    /// `(scroll_row, scroll_offset)`. Restored with the selections, so a
    /// pattern that selects nothing glides the view back to where it started.
    origin_view: (u32, f32),
    /// The pattern the live preview last ran for.
    ///
    /// The preview syncs once per frame, so without this every idle frame
    /// redoes the restore and the transform for a pattern that has not changed.
    previewed: Option<String>,
}

pub(super) fn open(stoat: &mut Stoat, kind: RegexSelectKind) -> UpdateEffect {
    if stoat.split_selection_input.is_some() {
        return UpdateEffect::None;
    }
    let (origin_selections, origin_view) = search::pane_editor(stoat)
        .map(|editor| {
            (
                editor.selections.shared_anchors(),
                (editor.scroll_row, editor.scroll_offset),
            )
        })
        .unwrap_or((Arc::from([]), (0, 0.0)));

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::SplitSelection, "", "insert");
    stoat.split_selection_input = Some(SplitSelectionInputState {
        input,
        kind,
        origin_selections,
        origin_view,
        previewed: None,
    });
    UpdateEffect::Redraw
}

/// Bring the open regex prompt's live preview up to date with what is typed.
///
/// A no-op when no prompt is open, or when the pattern has not changed since
/// the last sync. Otherwise the pane editor returns to the origin captured at
/// open and the prompt's transform runs from there, so the preview shown is
/// always the one that pattern alone produces.
///
/// The preview is silent. A pattern that is empty, does not compile yet, or
/// selects nothing is an ordinary state mid-typing, so it keeps the origin and
/// raises no status message the way a submit does.
pub(crate) fn sync_regex_preview(stoat: &mut Stoat) {
    let Some(state) = stoat.split_selection_input.as_ref() else {
        return;
    };
    let pattern = state.input.text(stoat.active_workspace());
    if state.previewed.as_deref() == Some(pattern.as_str()) {
        return;
    }

    let state = stoat
        .split_selection_input
        .as_mut()
        .expect("prompt still open");
    state.previewed = Some(pattern.clone());
    let kind = state.kind;
    let selections = Arc::clone(&state.origin_selections);
    let view = state.origin_view;

    search::restore_origin(stoat, selections, view);

    let Some(regex) = (!pattern.is_empty())
        .then(|| search::compile_cursor_regex(&pattern, search::smart_case(stoat)))
        .flatten()
    else {
        return;
    };
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = search::pane_editor(stoat) {
        apply_kind(editor, kind, &regex, scrolloff);
    }
}

/// Submit the regex modal. Reads the typed pattern, compiles it, and either
/// splits every selection at each match or replaces the selections with the
/// matches, per the modal's kind. Empty pattern, invalid regex, or no enclosing
/// editor close the input without changing selections. Returns `true` when the
/// input modal was open.
///
/// The transform runs from the selections the prompt opened over, not from the
/// preview on screen, so Enter lands what the last preview showed.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    search::restore_origin(stoat, state.origin_selections, state.origin_view);

    if query.is_empty() {
        return true;
    }
    let smart_case = search::smart_case(stoat);
    let Some(regex) = search::compile_cursor_regex(&query, smart_case) else {
        stoat.set_status(format!("invalid regex: {query}"));
        return true;
    };
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = search::pane_editor(stoat)
        && !apply_kind(editor, state.kind, &regex, scrolloff)
    {
        stoat.set_status("nothing selected");
    }
    true
}

/// Apply `kind`'s transform to `editor` and scroll its primary into view.
///
/// Returns `false` when a select found no match, which leaves the selections
/// and the view as they were. A split always applies, since a selection with
/// no match splits into itself.
fn apply_kind(
    editor: &mut EditorState,
    kind: RegexSelectKind,
    regex: &CursorRegex,
    scrolloff: u32,
) -> bool {
    let applied = match kind {
        RegexSelectKind::Split => {
            split_on_matches(editor, regex);
            true
        },
        RegexSelectKind::Select => select_on_matches(editor, regex),
    };
    if applied {
        view::follow_jump(editor, scrolloff);
    }
    applied
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
fn split_on_matches(editor: &mut EditorState, regex: &CursorRegex) {
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
            for m in regex.find_iter(rope.regex_input(start..end)) {
                // Two matches touching leave nothing between them, and that gap
                // is still a position the user split at, so it becomes a cursor
                // rather than disappearing.
                pieces.push((piece_start, m.start()));
                piece_start = m.end();
            }
            if piece_start < end {
                pieces.push((piece_start, end));
            }
            widen_pieces(rope, pieces)
        });
    editor.selections.make_first_primary();
}

/// Replace the selections with every match found inside them.
///
/// Returns `false` and keeps the selections when nothing matches anywhere, so
/// the caller decides what an empty result says.
fn select_on_matches(editor: &mut EditorState, regex: &CursorRegex) -> bool {
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    // Collected in one pass rather than asked twice. Whether anything matched
    // is a property of what was collected, so a scan of its own to answer that
    // would be the same work over the same ranges.
    let found: Vec<Vec<(usize, usize)>> = editor
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
                .find_iter(rope.regex_input(start..end))
                // A zero-width match on the selection's own end sits just
                // outside what was searched, which is what an anchor like
                // `$` produces. Every other empty match names a position
                // inside it and becomes a cursor there.
                .filter(|m| m.start() != end || m.start() != m.end())
                .map(|m| (m.start(), m.end()))
                .collect();
            widen_pieces(rope, matched)
        })
        .collect();

    if found.iter().all(Vec::is_empty) {
        return false;
    }

    // The whole set is replaced rather than each selection being split in
    // place, so a selection that matched nothing is gone rather than left
    // sitting where the user searched and found nothing. The guard above
    // returned already if that were every selection, so this is never empty.
    let matches: Vec<(usize, usize)> = found.into_iter().flatten().collect();
    editor
        .selections
        .replace_with_fresh_ids_from_offsets(&matches, Bias::Right, buffer_snapshot);
    editor.selections.make_first_primary();
    true
}

/// Cancel the input modal without splitting, putting the pane editor back where
/// the prompt found it. Returns `true` when the input modal was open.
pub(crate) fn cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    search::restore_origin(stoat, state.origin_selections, state.origin_view);
    true
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{dispatch, search},
        app::UpdateEffect,
        test_harness::{editor, keys, TestHarness},
        Stoat,
    };
    use crossterm::event::{Event, KeyCode};
    use stoat_action::{self as action, Action};

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

    /// The bar reads back the typed pattern behind a `select:` label, the way it
    /// reads back a search query behind `/`.
    #[test]
    fn the_select_prompt_shows_the_typed_pattern() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo bar");
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("fo");

        let prompt = search::prompt_display(&mut h.stoat).expect("the prompt is open");
        h.snapshot();
        assert_eq!(
            (
                prompt.label,
                prompt.text.as_str(),
                prompt.cursor,
                h.rendered_text().contains("select:fo"),
            ),
            ("select:", "fo", 2, true),
            "the prompt names the kind, the text, and the caret, and the bar paints them",
        );
    }

    #[test]
    fn the_split_prompt_is_labeled_split() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo bar");
        dispatch(&mut h.stoat, &action::SplitSelection);

        assert_eq!(
            search::prompt_display(&mut h.stoat).map(|prompt| prompt.label),
            Some("split:"),
        );
    }

    /// The pane editor's selection ranges, as `(start, end)` byte offsets.
    ///
    /// The focused editor is the prompt's own while it is open, so
    /// `editor::selection_spans` reads the wrong one here.
    fn pane_spans(h: &mut TestHarness) -> Vec<(usize, usize)> {
        let editor = search::pane_editor(&mut h.stoat).expect("pane holds an editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                (
                    buf_snap.resolve_anchor(&sel.start),
                    buf_snap.resolve_anchor(&sel.end),
                )
            })
            .collect()
    }

    /// Types into the open prompt and runs the preview a frame runs.
    fn preview(h: &mut TestHarness, text: &str) {
        h.type_text(text);
        super::sync_regex_preview(&mut h.stoat);
    }

    /// `text` selected whole, with the regex prompt `action` opens over it.
    fn prompt_over(text: &str, action: &dyn Action) -> TestHarness {
        let mut h = Stoat::test();
        h.seed_focused_buffer(text);
        select_range(&mut h, 0, text.len());
        dispatch(&mut h.stoat, action);
        h
    }

    #[test]
    fn typing_a_select_pattern_previews_its_matches() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "foo");
        assert_eq!(pane_spans(&mut h), [(0, 3), (8, 11)]);
    }

    /// Each sync starts from the origin rather than from the last preview, so
    /// `foo` finds the matches that the `fo` preview cut short, and backspace
    /// walks the preview back.
    #[test]
    fn each_keystroke_previews_from_the_origin() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "fo");
        let fo = pane_spans(&mut h);
        preview(&mut h, "o");
        let foo = pane_spans(&mut h);
        h.type_keys("backspace");
        super::sync_regex_preview(&mut h.stoat);

        assert_eq!(
            [fo, foo, pane_spans(&mut h)],
            [
                vec![(0, 2), (8, 10)],
                vec![(0, 3), (8, 11)],
                vec![(0, 2), (8, 10)]
            ],
        );
    }

    /// A pattern that does not compile yet is an ordinary state mid-typing, so
    /// the preview goes back to the origin and says nothing.
    #[test]
    fn a_partial_pattern_keeps_the_origin_silently() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "fo");
        preview(&mut h, "[");
        assert_eq!(
            (pane_spans(&mut h), h.stoat.pending_message.as_deref()),
            (vec![(0, 11)], None),
        );
    }

    #[test]
    fn a_pattern_matching_nothing_keeps_the_origin_silently() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "foo");
        preview(&mut h, "zzz");
        assert_eq!(
            (pane_spans(&mut h), h.stoat.pending_message.as_deref()),
            (vec![(0, 11)], None),
        );
    }

    #[test]
    fn escape_after_a_preview_restores_the_origin() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "foo");
        h.type_keys("escape");
        assert_eq!(
            (h.stoat.split_selection_input.is_some(), pane_spans(&mut h)),
            (false, vec![(0, 11)]),
        );
    }

    #[test]
    fn enter_keeps_the_preview_landing() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "foo");
        let previewed = pane_spans(&mut h);
        h.type_keys("enter");
        assert_eq!(
            (
                h.stoat.split_selection_input.is_some(),
                previewed,
                pane_spans(&mut h)
            ),
            (false, vec![(0, 3), (8, 11)], vec![(0, 3), (8, 11)]),
        );
    }

    /// Keys typed faster than a frame reach Enter before their preview does,
    /// so Enter applies the whole pattern from the origin. Over the stale `fo`
    /// preview it finds no `foo` and selects nothing.
    ///
    /// The keys go through `update` alone, which runs no frame and so no sync.
    #[test]
    fn enter_ahead_of_the_preview_applies_the_whole_pattern() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "fo");
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('o'))));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(
            (pane_spans(&mut h), h.stoat.pending_message.as_deref()),
            (vec![(0, 3), (8, 11)], None),
        );
    }

    #[test]
    fn typing_a_split_pattern_previews_the_pieces() {
        let mut h = prompt_over("abc 123 def", &action::SplitSelection);
        preview(&mut h, "\\d+");
        assert_eq!(pane_spans(&mut h), [(0, 4), (7, 11)]);
    }

    /// The preview follows its primary below the view, and Enter keeps the view
    /// there rather than gliding back to where the prompt opened.
    ///
    /// The frame runs the sync from `drive_background`, which this test calls in
    /// place of the sync to cover that path.
    #[test]
    fn a_preview_scrolls_to_its_match_and_enter_keeps_it() {
        let text: String = (0..100)
            .map(|row| match row {
                80 => "needle\n".to_string(),
                _ => format!("line {row}\n"),
            })
            .collect();
        let mut h = prompt_over(&text, &action::SelectRegex);
        h.type_text("needle");
        h.stoat.drive_background();

        let scroll = |h: &mut TestHarness| {
            search::pane_editor(&mut h.stoat)
                .expect("pane holds an editor")
                .scroll_row
        };
        let previewed = scroll(&mut h);
        h.type_keys("enter");
        assert_eq!(
            (previewed > 0, scroll(&mut h)),
            (true, previewed),
            "the preview scrolled to the match, and Enter kept that scroll",
        );
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
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("invalid regex: [unclosed"),
            "the pattern that failed is quoted back",
        );
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

    /// The first match is the primary, so keeping the primary keeps the front.
    ///
    /// The collection reads the primary off the highest id and the producer
    /// mints ascending. Without the hand-off the last match wins, which leaves
    /// the next single-selection action working at the far end of what the user
    /// searched.
    #[test]
    fn selecting_leaves_the_primary_on_the_first_match() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo bar foo");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("foo");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        dispatch(&mut h.stoat, &action::KeepPrimarySelection);
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 3, false)],
            "the front match survives, not the last",
        );
    }

    /// A split hands the primary to its first piece too.
    #[test]
    fn splitting_leaves_the_primary_on_the_first_piece() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("foo,bar,baz");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text(",");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        dispatch(&mut h.stoat, &action::KeepPrimarySelection);
        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(0, 3, false)],
            "the first piece survives",
        );
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

    /// A selection is matched where it sits in the buffer, so `^` asks whether
    /// a line really starts there.
    ///
    /// Matching it as a detached string makes the selection's own start count
    /// as a line start, and an anchor then means something different depending
    /// on where the user happened to cut. The search is bounded to the range
    /// with the rest of the buffer still around it.
    #[test]
    fn an_anchor_does_not_match_part_way_along_a_line() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abcdef\n");
        select_range(&mut h, 3, 6);

        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("^d");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(3, 6, false)],
            "no line starts at the selection, so nothing matched"
        );
        assert_eq!(h.stoat.pending_message.as_deref(), Some("nothing selected"));
    }

    /// A word boundary at the selection's edge consults the character outside
    /// it, which is the other half of matching in place.
    #[test]
    fn a_word_boundary_sees_the_character_before_the_selection() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abcdef ghi\n");
        select_range(&mut h, 3, 10);

        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("\\bdef");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        assert_eq!(
            editor::selection_spans(&mut h.stoat),
            vec![(3, 10, false)],
            "the c before the cut is a word character, so no boundary sits there"
        );
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
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("invalid regex: [unclosed"),
            "the pattern that failed is quoted back",
        );
    }
}
