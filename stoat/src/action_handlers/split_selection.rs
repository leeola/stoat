use crate::{
    action_handlers::{
        search::{self, CursorRegex},
        view,
    },
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    input_view::{InputView, SubmitTarget},
    jumplist::JumpEntry,
};
use std::{collections::VecDeque, sync::Arc};
use stoat_text::{next_char_boundary, Anchor, Bias, Rope, Selection};

/// The most previews a prompt keeps for a return to their pattern.
///
/// Each one holds a selection per match, so a prompt typed through a long
/// pattern keeps the latest few rather than every one.
const SHOWN_PREVIEWS: usize = 8;

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
    /// `(scroll_row, scroll_offset)`. Restored when the pattern is empty or
    /// does not compile, when a select finds no match, and on cancel. Any other
    /// pattern leaves the view to follow its landing from where it stands.
    origin_view: (u32, f32),
    /// The jumplist entry for the origin, captured before the prompt took
    /// focus. `None` when no editor was focused.
    ///
    /// [`submit`] pushes this once the pattern compiles, whether or not it
    /// matches, as the Helix regex prompt does.
    origin_jump: Option<JumpEntry>,
    /// The pattern the live preview last ran for.
    ///
    /// The preview syncs once per frame, so without this every idle frame
    /// redoes the restore and the transform for a pattern that has not changed.
    previewed: Option<String>,
    /// The previews shown since the prompt opened, newest first.
    ///
    /// A Backspace back to an earlier pattern installs the set that pattern
    /// showed rather than scanning the selections again.
    shown: VecDeque<ShownPreview>,
}

/// The set that one pattern's preview installed, kept for a return to the
/// pattern.
struct ShownPreview {
    pattern: String,
    /// The version of the buffer the preview ran over. An edit while the prompt
    /// is open changes what the pattern matches.
    version: u64,
    /// `None` when the pattern selected nothing and the preview showed the
    /// origin.
    selections: Option<Arc<[Selection<Anchor>]>>,
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
    // Captured before InputView::create moves focus onto the prompt's own
    // editor.
    let origin_jump = super::focused_editor_mut(stoat).map(super::jump::live_entry);

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::SplitSelection, "", "insert");
    stoat.split_selection_input = Some(SplitSelectionInputState {
        input,
        kind,
        origin_selections,
        origin_view,
        origin_jump,
        previewed: None,
        shown: VecDeque::new(),
    });
    UpdateEffect::Redraw
}

/// Bring the open regex prompt's live preview up to date with what is typed.
///
/// A no-op when no prompt is open, or when the pattern has not changed since
/// the last sync. Otherwise the prompt's transform runs again from the
/// selections captured at open, so the preview shown is always the one that
/// pattern alone produces. The view follows the result from where it stands.
///
/// A pattern that the prompt showed before, over the same buffer, installs the
/// set it showed then, so a Backspace back to it scans nothing.
///
/// The preview is silent. A pattern that is empty, does not compile yet, or
/// selects nothing is an ordinary state mid-typing. It glides the view back to
/// the origin and raises no status message the way a submit does.
pub(crate) fn sync_regex_preview(stoat: &mut Stoat) {
    let Some(state) = stoat.split_selection_input.as_ref() else {
        return;
    };
    let pattern = state.input.text(stoat.active_workspace());
    if state.previewed.as_deref() == Some(pattern.as_str()) {
        return;
    }
    let version =
        search::pane_editor(stoat).map(|editor| editor.display_map.buffer_snapshot().version());

    let state = stoat
        .split_selection_input
        .as_mut()
        .expect("prompt still open");
    state.previewed = Some(pattern.clone());
    let kind = state.kind;
    let origin = Arc::clone(&state.origin_selections);
    let view = state.origin_view;
    let shown = version.and_then(|version| {
        state
            .shown
            .iter()
            .find(|shown| shown.version == version && shown.pattern == pattern)
            .map(|shown| shown.selections.clone())
    });

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let landed = match shown {
        Some(shown) => show_again(stoat, origin, shown, scrolloff),
        None => preview_from_origin(stoat, pattern, kind, origin, version, scrolloff),
    };
    if !landed {
        search::restore_origin_view(stoat, view);
    }
}

/// Install the set that a pattern showed before, or the origin when it selected
/// nothing, and report whether a set landed.
fn show_again(
    stoat: &mut Stoat,
    origin: Arc<[Selection<Anchor>]>,
    shown: Option<Arc<[Selection<Anchor>]>>,
    scrolloff: u32,
) -> bool {
    let Some(selections) = shown else {
        restore_origin_if_moved(stoat, origin);
        return false;
    };
    if let Some(editor) = search::pane_editor(stoat) {
        editor.selections.restore(selections);
        view::follow_jump(editor, scrolloff);
    }
    true
}

/// Run the prompt's transform for `pattern` from the origin, keep what it
/// showed against the buffer `version`, and report whether a set landed.
fn preview_from_origin(
    stoat: &mut Stoat,
    pattern: String,
    kind: RegexSelectKind,
    origin: Arc<[Selection<Anchor>]>,
    version: Option<u64>,
    scrolloff: u32,
) -> bool {
    let regex = (!pattern.is_empty())
        .then(|| search::compile_cursor_regex(&pattern, search::smart_case(stoat)))
        .flatten();
    restore_origin_if_moved(stoat, origin);

    let Some(regex) = regex else {
        return false;
    };
    let Some(editor) = search::pane_editor(stoat) else {
        return false;
    };
    let landed = apply_kind(editor, kind, &regex, scrolloff);
    let selections = landed.then(|| editor.selections.shared_anchors());
    if let (Some(version), Some(state)) = (version, stoat.split_selection_input.as_mut()) {
        state.shown.push_front(ShownPreview {
            pattern,
            version,
            selections,
        });
        state.shown.truncate(SHOWN_PREVIEWS);
    }
    landed
}

/// Submit the regex modal. Reads the typed pattern, compiles it, and either
/// splits every selection at each match or replaces the selections with the
/// matches, per the modal's kind. Empty pattern, invalid regex, or no enclosing
/// editor close the input without changing selections. Returns `true` when the
/// input modal was open.
///
/// The transform runs from the selections the prompt opened over, not from the
/// preview on screen, so Enter lands what the last preview showed.
///
/// The view stays where the preview left it. An empty or invalid pattern, or a
/// select that finds nothing, glides it back to the origin.
///
/// A pattern that compiles records the origin on the jumplist, so a jump back
/// returns to the selections the prompt opened over, even after a select that
/// found nothing.
pub(crate) fn submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.split_selection_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    restore_origin_if_moved(stoat, state.origin_selections);

    if query.is_empty() {
        search::restore_origin_view(stoat, state.origin_view);
        return true;
    }
    let smart_case = search::smart_case(stoat);
    let Some(regex) = search::compile_cursor_regex(&query, smart_case) else {
        search::restore_origin_view(stoat, state.origin_view);
        stoat.set_status(format!("invalid regex: {query}"));
        return true;
    };
    if let Some(entry) = state.origin_jump {
        super::jump::push_entry(stoat, entry);
    }
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = search::pane_editor(stoat)
        && !apply_kind(editor, state.kind, &regex, scrolloff)
    {
        search::restore_origin_view(stoat, state.origin_view);
        stoat.set_status("nothing selected");
    }
    true
}

/// Put the pane editor's selections back to `origin` unless they are that set
/// already.
///
/// A restore installs a new generation whatever the list held, and the pane
/// repaints on any new generation. A key whose pattern is empty or does not
/// compile after another such key finds the origin in place, so the pane
/// replays instead.
fn restore_origin_if_moved(stoat: &mut Stoat, origin: Arc<[Selection<Anchor>]>) {
    let Some(editor) = search::pane_editor(stoat) else {
        return;
    };
    if !Arc::ptr_eq(&editor.selections.shared_anchors(), &origin) {
        editor.selections.restore(origin);
    }
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
    let offsets = {
        let anchors: Vec<Anchor> = editor.selections.anchors().copied().collect();
        buffer_snapshot.resolve_anchors_batch(&anchors)
    };
    let spans = offsets.as_chunks::<2>().0;

    // A zero-width selection splits into itself, and only the general path
    // keeps it as it was, its anchors and its goal included.
    if spans.iter().any(|span| span[0] == span[1]) {
        editor
            .selections
            .split_each(buffer_snapshot, Bias::Right, |sel| {
                let start = buffer_snapshot.resolve_anchor(&sel.start);
                let end = buffer_snapshot.resolve_anchor(&sel.end);
                match start == end {
                    true => Vec::new(),
                    false => split_pieces(rope, regex, start, end),
                }
            });
        editor.selections.make_first_primary();
        return;
    }

    // The selections are disjoint and in document order, and each piece stays
    // inside its selection, so the pieces arrive in order.
    let pieces: Vec<(usize, usize)> = spans
        .iter()
        .flat_map(|span| split_pieces(rope, regex, span[0], span[1]))
        .collect();
    editor
        .selections
        .replace_sorted_from_offsets(&pieces, Bias::Right, buffer_snapshot);
}

/// The pieces that a split at each match of `regex` cuts `start..end` into,
/// with each zero-width piece widened.
fn split_pieces(rope: &Rope, regex: &CursorRegex, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    let mut piece_start = start;
    for m in regex.find_iter(rope.regex_input(start..end)) {
        // Two matches touching leave nothing between them, and that gap is
        // still a position the user split at, so it becomes a cursor rather
        // than disappearing.
        pieces.push((piece_start, m.start()));
        piece_start = m.end();
    }
    if piece_start < end {
        pieces.push((piece_start, end));
    }
    widen_pieces(rope, pieces)
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
    // A scan of the selections in document order finds the matches in order.
    let matches: Vec<(usize, usize)> = found.into_iter().flatten().collect();
    editor
        .selections
        .replace_sorted_from_offsets(&matches, Bias::Right, buffer_snapshot);
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
        editor_state::ScrollGlide,
        test_harness::{editor, keys, TestHarness},
        Stoat,
    };
    use crossterm::event::{Event, KeyCode};
    use std::sync::Arc;
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

    /// A key whose pattern is empty or does not compile leaves the selection set
    /// as it stood when the origin is in place already, so the pane replays. A
    /// failing key after a preview puts the origin back.
    #[test]
    fn a_failing_pattern_over_the_origin_keeps_the_selection_set() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        let identity = |h: &mut TestHarness| {
            search::pane_editor(&mut h.stoat)
                .expect("pane holds an editor")
                .selections
                .identity()
        };

        let mut identities = vec![identity(&mut h)];
        preview(&mut h, "(");
        identities.push(identity(&mut h));
        h.type_keys("backspace");
        super::sync_regex_preview(&mut h.stoat);
        identities.push(identity(&mut h));
        for text in ["foo", "[", "["] {
            preview(&mut h, text);
            identities.push(identity(&mut h));
        }

        let kept: Vec<bool> = identities
            .windows(2)
            .map(|pair| pair[0] == pair[1])
            .collect();
        assert_eq!(
            (kept, pane_spans(&mut h)),
            (vec![true, true, false, false, true], vec![(0, 11)]),
            "`(` and the empty pattern keep the open set, `foo` and `foo[` each \
             install one, and `foo[[` keeps the restored origin",
        );
    }

    /// A Backspace back to a pattern installs the very set that pattern showed,
    /// so the preview scans nothing.
    #[test]
    fn a_pattern_shown_before_installs_the_set_it_showed() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        let installed = |h: &mut TestHarness| {
            search::pane_editor(&mut h.stoat)
                .expect("pane holds an editor")
                .selections
                .shared_anchors()
        };
        preview(&mut h, "fo");
        let fo = installed(&mut h);
        preview(&mut h, "o");
        h.type_keys("backspace");
        super::sync_regex_preview(&mut h.stoat);

        assert_eq!(
            (Arc::ptr_eq(&installed(&mut h), &fo), pane_spans(&mut h)),
            (true, vec![(0, 2), (8, 10)]),
            "the Backspace back to `fo` installs the set `fo` showed",
        );
    }

    /// An edit while the prompt is open makes a return to a pattern scan
    /// again, since the edit changes what the pattern matches.
    #[test]
    fn an_edit_scans_a_shown_pattern_again() {
        let mut h = prompt_over("foo bar foo", &action::SelectRegex);
        preview(&mut h, "fo");
        preview(&mut h, "o");
        let buffer_id = search::pane_editor(&mut h.stoat)
            .expect("pane holds an editor")
            .buffer_id;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer exists")
            .write()
            .expect("buffer poisoned")
            .edit(4..4, "fo ");
        h.type_keys("backspace");
        super::sync_regex_preview(&mut h.stoat);

        assert_eq!(
            pane_spans(&mut h),
            vec![(0, 2), (4, 6), (11, 13)],
            "`fo` finds the match the edit inserted inside the selection",
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

    /// A hundred `line N` rows with `needle` on row 80, selected whole, with
    /// the select prompt open over them.
    fn needle_prompt() -> TestHarness {
        let text: String = (0..100)
            .map(|row| match row {
                80 => "needle\n".to_string(),
                _ => format!("line {row}\n"),
            })
            .collect();
        prompt_over(&text, &action::SelectRegex)
    }

    /// The pane editor's view, as `(scroll_row, scroll_offset, scroll_glide)`.
    fn pane_view(h: &mut TestHarness) -> (u32, f32, ScrollGlide) {
        let editor = search::pane_editor(&mut h.stoat).expect("pane holds an editor");
        (editor.scroll_row, editor.scroll_offset, editor.scroll_glide)
    }

    /// Finish the pane editor's glide the way the run loop's animation does,
    /// and return the view it rests at.
    fn finish_glide(h: &mut TestHarness) -> (u32, f32, ScrollGlide) {
        let editor = search::pane_editor(&mut h.stoat).expect("pane holds an editor");
        editor.scroll_offset = editor.scroll_row as f32;
        editor.scroll_glide = ScrollGlide::None;
        pane_view(h)
    }

    /// The preview follows its primary below the view. After that, neither a
    /// keystroke that keeps the match nor Enter moves the view.
    ///
    /// The frame runs the sync from `drive_background`, which this test calls in
    /// place of the sync to cover that path.
    #[test]
    fn a_preview_scrolls_to_its_match_and_enter_keeps_it() {
        let mut h = needle_prompt();
        h.type_text("need");
        h.stoat.drive_background();
        let landed = finish_glide(&mut h);

        h.type_text("le");
        h.stoat.drive_background();
        let typed = pane_view(&mut h);
        h.type_keys("enter");
        assert_eq!(
            (landed.0 > 0, typed, pane_view(&mut h)),
            (true, landed, landed),
            "the preview scrolled to the match, and the next key and Enter kept that view",
        );
    }

    /// A pattern that stops selecting anything glides the view back to where
    /// the prompt opened, along with the selections.
    #[test]
    fn a_pattern_that_stops_matching_glides_the_view_back() {
        let mut h = needle_prompt();
        preview(&mut h, "needle");
        let landed = finish_glide(&mut h);
        preview(&mut h, "x");
        assert_eq!(
            (landed.0 > 0, pane_view(&mut h)),
            (true, (0, landed.0 as f32, ScrollGlide::Page)),
            "the view glides from the landing back to the origin row",
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

    /// The number of entries on the focused pane's jumplist.
    fn jumplist_len(h: &TestHarness) -> usize {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).jumplist.entries().len()
    }

    /// The selections after `pattern` goes through the prompt `opener` opens
    /// over the whole of `abc 123 def`, and a jump back follows.
    fn submit_then_jump_back(opener: &dyn Action, pattern: &str) -> Vec<(usize, usize, bool)> {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        dispatch(&mut h.stoat, opener);
        h.type_text(pattern);
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        dispatch(&mut h.stoat, &action::JumpBackward);
        editor::selection_spans(&mut h.stoat)
    }

    /// Enter records the selection the prompt opened over, so a jump back
    /// returns to it, as Helix's regex prompt does.
    #[test]
    fn split_submit_records_the_origin_on_the_jumplist() {
        assert_eq!(
            submit_then_jump_back(&action::SplitSelection, "\\d+"),
            vec![(0, 11, false)],
        );
    }

    #[test]
    fn select_regex_submit_records_the_origin_on_the_jumplist() {
        assert_eq!(
            submit_then_jump_back(&action::SelectRegex, "\\d+"),
            vec![(0, 11, false)],
        );
    }

    /// A select that finds nothing still records the origin, since the push
    /// rides on the pattern compiling rather than on a match.
    #[test]
    fn select_regex_with_no_match_still_records_the_origin() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("abc 123 def");
        select_range(&mut h, 0, 11);
        let before = jumplist_len(&h);
        dispatch(&mut h.stoat, &action::SelectRegex);
        h.type_text("zzz");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert_eq!(jumplist_len(&h), before + 1);
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

    /// A split keeps a zero-width selection as it was, its goal included, and
    /// splits the others.
    #[test]
    fn splitting_keeps_a_zero_width_selection_as_it_was() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("a,b");
        select_range(&mut h, 0, 2);
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            let at_end = buf_snap.anchor_at(3, stoat_text::Bias::Right);
            let cursor = stoat_text::Selection {
                id: 0,
                start: at_end,
                end: at_end,
                reversed: false,
                goal: stoat_text::SelectionGoal::Column(4),
            };
            editor
                .selections
                .extend_with_fresh_ids(vec![cursor], buf_snap);
        }
        dispatch(&mut h.stoat, &action::SplitSelection);
        h.type_text(",");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));

        let goals: Vec<stoat_text::SelectionGoal> =
            crate::action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("editor")
                .selections
                .all_anchors()
                .iter()
                .map(|sel| sel.goal)
                .collect();
        assert_eq!(
            (editor::selection_spans(&mut h.stoat), goals),
            (
                vec![(0, 1, false), (3, 3, false)],
                vec![
                    stoat_text::SelectionGoal::None,
                    stoat_text::SelectionGoal::Column(4)
                ]
            ),
            "the first selection splits at the comma, and the cursor at the end keeps its goal",
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
