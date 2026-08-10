use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};
use regex_cursor::{engines::meta, regex_automata::util::syntax};
use stoat_text::Rope;

/// The search pattern compiled for the engine that matches over a rope.
pub(crate) type CursorRegex = meta::Regex;

/// Direction the search was opened in. Forward (`/`) finds matches at
/// or after the cursor; Reverse (`?`) finds matches before the cursor.
/// `SearchNext` repeats in this direction; `SearchPrev` repeats in the
/// opposite direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchDirection {
    Forward,
    Reverse,
}

impl SearchDirection {
    fn flipped(self) -> Self {
        match self {
            Self::Forward => Self::Reverse,
            Self::Reverse => Self::Forward,
        }
    }
}

/// Active state while the user is typing a search query into the
/// input modal. Disposed by [`search_submit`] / [`search_cancel`].
pub(crate) struct SearchInputState {
    pub(crate) input: InputView,
    pub(crate) direction: SearchDirection,
}

/// Persisted query + direction from the most recent submitted search.
///
/// `SearchNext` and `SearchPrev` consume this. It clears when the search input
/// is cancelled with an empty submit.
#[derive(Clone, Debug)]
pub(crate) struct LastSearch {
    pub(crate) query: String,
    pub(crate) direction: SearchDirection,
    /// [`Self::query`] compiled, so repeating the search costs a refcount bump
    /// rather than a rebuild on every press.
    ///
    /// `None` for a pattern that does not compile. The query is kept either
    /// way, because it is also what the search register pastes and what the
    /// highlight pass reads, neither of which needs it to be searchable.
    ///
    /// Compiled for the cursor engine rather than the plain one, so a repeat
    /// runs over the buffer's chunks instead of a flattened copy of it. The
    /// highlight pass compiles its own, over the window it paints.
    regex: Option<CursorRegex>,
}

impl LastSearch {
    pub(crate) fn new(query: String, direction: SearchDirection) -> Self {
        Self {
            regex: compile_cursor_regex(&query),
            query,
            direction,
        }
    }
}

pub(super) fn open_search_input(stoat: &mut Stoat) -> UpdateEffect {
    open_input(stoat, SearchDirection::Forward)
}

pub(super) fn open_reverse_search_input(stoat: &mut Stoat) -> UpdateEffect {
    open_input(stoat, SearchDirection::Reverse)
}

fn open_input(stoat: &mut Stoat, direction: SearchDirection) -> UpdateEffect {
    if stoat.search_input.is_some() {
        return UpdateEffect::None;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::Search, "", "insert", 1);
    stoat.search_input = Some(SearchInputState { input, direction });
    UpdateEffect::Redraw
}

/// Submit the search query: read the typed text, jump to the first
/// match in the chosen direction (with wrap), and store
/// [`LastSearch`] for `n` / `N` to repeat. Returns true when the
/// modal was open so the prompt-submit router can short-circuit.
pub(crate) fn search_submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.search_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let direction = state.direction;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    if query.is_empty() {
        return true;
    }

    let last = LastSearch::new(query, direction);
    let origin = super::jump::live_entry(stoat);
    if let Some(regex) = last.regex.as_ref()
        && jump_to_match(stoat, regex, direction)
        && let Some(entry) = origin
    {
        super::jump::push_entry(stoat, entry);
    }
    // The query reaches the frame directly rather than through any display
    // layer, so nothing else would report that the highlighted matches moved.
    stoat.paint_generation += 1;
    stoat.last_search = Some(last);
    true
}

/// Cancel the input modal without changing the cursor. Disposes
/// the embedded [`InputView`].
pub(crate) fn search_cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.search_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    true
}

pub(super) fn search_next(stoat: &mut Stoat) -> UpdateEffect {
    let Some((regex, direction)) = repeat_target(stoat) else {
        return UpdateEffect::None;
    };
    if jump_to_match(stoat, &regex, direction) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

pub(super) fn search_prev(stoat: &mut Stoat) -> UpdateEffect {
    let Some((regex, direction)) = repeat_target(stoat) else {
        return UpdateEffect::None;
    };
    if jump_to_match(stoat, &regex, direction.flipped()) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

/// The pattern and direction a repeat press searches with, or `None` when
/// nothing has been searched for or the stored pattern never compiled.
///
/// Clones the regex, which is a refcount bump, rather than the whole
/// [`LastSearch`], which would copy the query text on every press.
fn repeat_target(stoat: &Stoat) -> Option<(CursorRegex, SearchDirection)> {
    let last = stoat.last_search.as_ref()?;
    Some((last.regex.clone()?, last.direction))
}

/// Find the next match of `regex` in the focused editor's buffer,
/// starting from the primary cursor and walking in `direction` with
/// wrap-around, then move every selection's primary cursor to the match
/// start. Returns true when a match was found and the cursor moved.
///
/// Takes the pattern already compiled, since `n` and `N` repeat one that
/// [`LastSearch`] built when it was submitted.
fn jump_to_match(stoat: &mut Stoat, regex: &CursorRegex, direction: SearchDirection) -> bool {
    use crate::pane::View;
    use stoat_text::SelectionGoal;

    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return false,
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor();
    let cursor = stoat_text::cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    );

    let target = match direction {
        SearchDirection::Forward => find_forward(regex, rope, cursor),
        SearchDirection::Reverse => find_reverse(regex, rope, cursor),
    };
    let Some(target) = target else { return false };

    super::movement::move_cursors(&mut editor.selections, buffer_snapshot, false, |_| {
        Some((target, SelectionGoal::None))
    });
    true
}

fn find_forward(regex: &CursorRegex, rope: &Rope, head: usize) -> Option<usize> {
    let start = head.saturating_add(1).min(rope.len());
    if let Some(m) = next_match_at_or_after(regex, rope, start) {
        return Some(m);
    }
    next_match_at_or_after(regex, rope, 0)
}

/// The last match starting before `head`, or the last in the file when there is
/// none, so a reverse search wraps around.
///
/// One forward pass, stopping at the first match the cursor has already passed.
/// `find_iter` is lazy, so an ordinary reverse search reads `head` bytes rather
/// than the file. Only a search with nothing behind the cursor, which is the
/// one that has to wrap, reads to the end.
fn find_reverse(regex: &CursorRegex, rope: &Rope, head: usize) -> Option<usize> {
    let mut matches = regex.find_iter(rope.regex_input(0..rope.len()));
    let mut before = None;

    let at_or_after = loop {
        match matches.next() {
            Some(m) if m.start() < head => before = Some(m.start()),
            other => break other,
        }
    };

    if before.is_some() {
        return before;
    }
    let first = at_or_after?;
    Some(matches.last().map_or(first.start(), |m| m.start()))
}

/// Finds the first regex match whose start is at or after `at`.
///
/// The scan is bounded to `at..` on the input rather than by slicing, so the
/// offset it reports is a buffer offset. A zero-width pattern can still report
/// a match before the bound, which is why the start is checked.
fn next_match_at_or_after(regex: &CursorRegex, rope: &Rope, at: usize) -> Option<usize> {
    if at > rope.len() {
        return None;
    }
    let m = regex.find(rope.regex_input(at..rope.len()))?;
    if m.start() >= at {
        Some(m.start())
    } else {
        None
    }
}

/// Compile `pattern` into a [`regex::Regex`] with multiline mode on,
/// so `^` and `$` match line boundaries inside the buffer text.
pub(crate) fn compile_search_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern).multi_line(true).build()
}

/// Compile `pattern` for the engine that matches over a rope's chunks, with the
/// same multiline mode [`compile_search_regex`] uses.
///
/// The two exist side by side because they suit different haystacks. This one
/// searches the buffer without flattening it. The plain one is what the
/// highlight pass runs over the window it has already built as a string.
pub(crate) fn compile_cursor_regex(pattern: &str) -> Option<CursorRegex> {
    CursorRegex::builder()
        .syntax(syntax::Config::new().multi_line(true))
        .build(pattern)
        .ok()
}

#[cfg(test)]
mod tests {
    use crate::test_harness::TestHarness;
    use std::path::PathBuf;
    use stoat_action::{self as action, OpenFile};

    fn seed(h: &mut TestHarness, contents: &str) -> PathBuf {
        let root = PathBuf::from("/search-test");
        let path = root.join("buf.txt");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buf_snap.rope(),
            buf_snap.resolve_anchor(&sel.tail()),
            buf_snap.resolve_anchor(&sel.head()),
        )
    }

    fn cached_match_count(h: &mut TestHarness) -> usize {
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .search_match_cache
            .as_ref()
            .expect("search active so the render populated the match cache")
            .matches
            .len()
    }

    #[test]
    fn forward_search_jumps_to_first_match_after_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenSearchInput);
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn forward_search_wraps_when_no_match_after_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        h.type_keys("l l l l l");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn reverse_search_jumps_to_first_match_before_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("l l l l l l l l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenReverseSearchInput);
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn reverse_search_wraps_when_no_match_before_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenReverseSearchInput);
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn search_next_repeats_forward_search() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 8);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    /// The buffer text from the cursor onward, which says whether a jump landed
    /// on a match rather than at an offset the match used to be at.
    fn text_at_cursor(h: &mut TestHarness) -> String {
        let at = cursor_offset(h);
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        rope.slice(at..rope.len()).to_string()
    }

    #[test]
    fn a_repeat_after_an_edit_searches_the_edited_buffer() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");

        // Shifts every following match along, so a repeat reading anything but
        // the buffer as it stands now lands short of one.
        h.type_keys("i");
        h.type_text("zz");
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);

        assert!(
            text_at_cursor(&mut h).starts_with("abc"),
            "the repeat landed on a match in the edited buffer, not where one used to be"
        );
    }

    /// A forward search resumes from the byte after the cursor, which is inside
    /// the character whenever the cursor sits on a multibyte one.
    ///
    /// The scan is handed that mid-character byte as its lower bound and still
    /// reports the next match, rather than panicking on it or stepping over a
    /// match to reach a boundary. Every other search test is ASCII, where the
    /// byte after the cursor is always a character boundary and this cannot
    /// come up.
    #[test]
    fn find_forward_resumes_from_inside_a_multibyte_character() {
        let text = stoat_text::Rope::from("\u{FC}ber \u{FC}ber");
        let regex = super::compile_cursor_regex("\u{FC}ber").expect("valid regex");

        // The cursor is on the two-byte umlaut at 0, so the scan starts at 1.
        assert_eq!(
            super::find_forward(&regex, &text, 0),
            Some(6),
            "a start inside the umlaut still reaches the next match",
        );
        assert_eq!(
            super::find_forward(&regex, &text, 6),
            Some(0),
            "and with nothing after it, wraps rather than stalling there",
        );

        // Four bytes wide, so the scan starts three bytes into the character.
        let wide = stoat_text::Rope::from("\u{1F600}x\u{1F600}x");
        let regex = super::compile_cursor_regex("x").expect("valid regex");
        assert_eq!(super::find_forward(&regex, &wide, 0), Some(4));
        assert_eq!(super::find_forward(&regex, &wide, 5), Some(9));
    }

    /// Case-insensitive search folds non-ASCII pairs, not just ASCII ones.
    #[test]
    fn find_forward_folds_non_ascii_case() {
        let text = stoat_text::Rope::from("a\u{FC}b");
        let regex = super::compile_cursor_regex("(?i)\u{DC}").expect("valid regex");
        assert_eq!(
            super::find_forward(&regex, &text, 0),
            Some(1),
            "an uppercase umlaut pattern matches the lowercase one",
        );

        let sensitive = super::compile_cursor_regex("\u{DC}").expect("valid regex");
        assert_eq!(
            super::find_forward(&sensitive, &text, 0),
            None,
            "and without the flag it does not, so the fold is what matched",
        );
    }

    /// An empty pattern reports a character boundary, never a byte inside a
    /// character.
    ///
    /// It matches everywhere, so the offset it reports is decided entirely by
    /// where the scan is allowed to start. From a cursor on a multibyte
    /// character that lower bound is mid-character, and what comes back is the
    /// boundary after it.
    #[test]
    fn find_forward_with_an_empty_pattern_reports_a_boundary() {
        let text = stoat_text::Rope::from("\u{FC}ber \u{FC}ber");
        let regex = super::compile_cursor_regex("").expect("valid regex");
        assert_eq!(
            super::find_forward(&regex, &text, 0),
            Some(2),
            "the boundary after the umlaut, not the byte inside it",
        );
    }

    /// The reverse walk keeps only two candidates, so pin both the ordinary
    /// pick and the wrap-around against a buffer with matches either side of
    /// the cursor.
    #[test]
    fn find_reverse_picks_the_last_match_before_the_cursor() {
        let regex = super::compile_cursor_regex("ab").expect("valid regex");
        let text = stoat_text::Rope::from("ab..ab..ab");

        assert_eq!(
            super::find_reverse(&regex, &text, 8),
            Some(4),
            "the nearest match before the cursor wins, not the first or last",
        );
        assert_eq!(super::find_reverse(&regex, &text, 9), Some(8));
        assert_eq!(super::find_reverse(&regex, &text, 5), Some(4));
        assert_eq!(
            super::find_reverse(&regex, &text, 0),
            Some(8),
            "with nothing before the cursor it wraps to the last match",
        );
        assert_eq!(
            super::find_reverse(&regex, &stoat_text::Rope::from("nope"), 2),
            None
        );
    }

    /// Chunks are far smaller than the file, so a match near the end of one
    /// spans the boundary into the next. Nothing else here reaches past a
    /// single chunk, so without this the chunk walking is untested.
    #[test]
    fn a_match_straddling_a_chunk_boundary_is_found_either_way() {
        let mut text = String::new();
        for row in 0..80 {
            text.push_str(&format!("filler line {row} with some words on it\n"));
        }
        let at = text.len();
        text.push_str("the needle_in_a_haystack sits here\n");
        for row in 0..80 {
            text.push_str(&format!("trailing line {row}\n"));
        }

        let rope = stoat_text::Rope::from(text.as_str());
        let regex = super::compile_cursor_regex("needle_in_a_haystack").expect("valid regex");
        let target = at + "the ".len();

        assert!(
            rope.chunks().count() > 1,
            "the fixture has to span chunks for this to test anything"
        );
        assert_eq!(
            super::find_forward(&regex, &rope, 0),
            Some(target),
            "found walking forward from the start"
        );
        assert_eq!(
            super::find_reverse(&regex, &rope, rope.len()),
            Some(target),
            "and walking back from the end"
        );
        assert_eq!(
            super::find_forward(&regex, &rope, target),
            Some(target),
            "and wrapping round when the cursor sits on it"
        );
    }

    #[test]
    fn search_prev_flips_direction() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 8);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    /// A reverse repeat with nothing behind the cursor wraps to the file's last
    /// match. That is the one case reading past the cursor, so it is the one the
    /// early stop could get wrong.
    #[test]
    fn search_prev_from_the_top_wraps_to_the_last_match() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 8);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(cursor_offset(&mut h), 0, "back to the first match");

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(
            cursor_offset(&mut h),
            16,
            "nothing lies before the top, so it wraps to the last match",
        );
    }

    /// A pattern that does not compile finds nothing, but it is still what the
    /// user typed, and the search register pastes it back.
    #[test]
    fn an_uncompilable_pattern_still_stores_its_query() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        let before = cursor_offset(&mut h);

        h.type_keys("/");
        h.type_text("[");
        h.type_keys("enter");

        assert_eq!(cursor_offset(&mut h), before, "no match, so no jump");
        assert_eq!(
            h.stoat.last_search.as_ref().map(|s| s.query.as_str()),
            Some("["),
        );
        assert_eq!(
            crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext),
            crate::app::UpdateEffect::None,
            "repeating a pattern that never compiled stays a no-op",
        );
    }

    /// Submitting a search moves the paint generation, and moving the cursor
    /// does not.
    ///
    /// The query is read straight off the app when a frame is built, so no
    /// display layer reports that the highlighted matches changed. A motion
    /// changes what is selected, which the display map already answers for.
    #[test]
    fn submitting_a_search_moves_the_paint_generation() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\nabc\n");
        let before = h.stoat.paint_generation;

        h.type_keys("j");
        assert_eq!(
            h.stoat.paint_generation, before,
            "a cursor motion paints from the display map alone",
        );

        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(
            h.stoat.paint_generation,
            before + 1,
            "a submitted query changes which matches are highlighted",
        );
    }

    #[test]
    fn no_match_leaves_cursor_unchanged() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        let before = cursor_offset(&mut h);
        h.type_keys("/");
        h.type_text("zzz");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), before);
        assert_eq!(
            h.stoat.last_search.as_ref().map(|s| s.query.as_str()),
            Some("zzz"),
        );
    }

    #[test]
    fn empty_submit_does_not_store_last_search() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        let before = cursor_offset(&mut h);
        h.type_keys("/");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), before);
        assert!(h.stoat.last_search.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn escape_cancels_without_jump() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        let before = cursor_offset(&mut h);
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("escape");
        assert_eq!(cursor_offset(&mut h), before);
        assert!(h.stoat.last_search.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn search_next_without_prior_search_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn snapshot_search_match_highlight() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        h.assert_snapshot("search_match_highlight");
    }

    #[test]
    fn regex_pattern_matches_first_occurrence() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc 123 def 456\n");
        h.type_keys("/");
        h.type_text("\\d+");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 4);
    }

    #[test]
    fn regex_anchors_match_only_at_line_start() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xfoo\nfoo bar\n");
        h.type_keys("/");
        h.type_text("^foo");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), 5);
    }

    #[test]
    fn invalid_regex_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        let before = cursor_offset(&mut h);
        h.type_keys("/");
        h.type_text("[unclosed");
        h.type_keys("enter");
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn snapshot_regex_variable_length_match_highlight() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc 1 22 333 4444 end\n");
        h.type_keys("/");
        h.type_text("\\d+");
        h.type_keys("enter");
        h.assert_snapshot("regex_variable_length_match_highlight");
    }

    #[test]
    fn search_match_cache_recomputes_on_query_change() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "aaa\n");

        h.type_keys("/");
        h.type_text("aa");
        h.type_keys("enter");
        assert_eq!(
            cached_match_count(&mut h),
            1,
            "non-overlapping 'aa' in 'aaa' matches once",
        );

        h.type_keys("/");
        h.type_text("a");
        h.type_keys("enter");
        assert_eq!(
            cached_match_count(&mut h),
            3,
            "changing the query recomputes the cache: 'a' matches three times",
        );
    }
}
