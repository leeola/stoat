use crate::{
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};

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
    regex: Option<regex::Regex>,
}

impl LastSearch {
    pub(crate) fn new(query: String, direction: SearchDirection) -> Self {
        Self {
            regex: compile_search_regex(&query).ok(),
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
fn repeat_target(stoat: &Stoat) -> Option<(regex::Regex, SearchDirection)> {
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
fn jump_to_match(stoat: &mut Stoat, regex: &regex::Regex, direction: SearchDirection) -> bool {
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
    // The regex needs one flat &str, so reuse the last one taken at this
    // version rather than re-flattening the whole file on every n/N.
    let version = buffer_snapshot.version();
    let text = match &editor.search_text_cache {
        Some((cached, text)) if *cached == version => text.clone(),
        _ => {
            let text = std::sync::Arc::new(rope.to_string());
            editor.search_text_cache = Some((version, text.clone()));
            text
        },
    };
    let sel = editor.selections.newest_anchor();
    let cursor = stoat_text::cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    );
    let len = text.len();

    let target = match direction {
        SearchDirection::Forward => find_forward(regex, &text, cursor, len),
        SearchDirection::Reverse => find_reverse(regex, &text, cursor),
    };
    let Some(target) = target else { return false };

    let new_buf = buffer_snapshot;
    editor.selections.transform(new_buf, |sel| {
        super::movement::land_block_cursor(sel.id, target, SelectionGoal::None, rope, new_buf)
    });
    true
}

fn find_forward(regex: &regex::Regex, text: &str, head: usize, len: usize) -> Option<usize> {
    let start = head.saturating_add(1).min(len);
    if let Some(m) = next_match_at_or_after(regex, text, start) {
        return Some(m);
    }
    next_match_at_or_after(regex, text, 0)
}

/// The last match starting before `head`, or the last in the file when there is
/// none, so a reverse search wraps around.
///
/// One forward pass, stopping at the first match the cursor has already passed.
/// `find_iter` is lazy, so an ordinary reverse search reads `head` bytes rather
/// than the file. Only a search with nothing behind the cursor, which is the
/// one that has to wrap, reads to the end.
fn find_reverse(regex: &regex::Regex, text: &str, head: usize) -> Option<usize> {
    let mut matches = regex.find_iter(text);
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
/// Walks forward via `find_at` and skips matches that pre-date `at`
/// (which can happen for zero-width patterns).
fn next_match_at_or_after(regex: &regex::Regex, text: &str, at: usize) -> Option<usize> {
    if at > text.len() {
        return None;
    }
    let m = regex.find_at(text, at)?;
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

    /// The buffer version the search text was flattened at, or `None` when
    /// nothing is cached.
    fn cached_text_version(h: &mut TestHarness) -> Option<u64> {
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .search_text_cache
            .as_ref()
            .map(|(version, _)| *version)
    }

    #[test]
    fn repeat_jumps_reuse_the_flattened_buffer() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        assert_eq!(
            cached_text_version(&mut h),
            None,
            "nothing is flattened before a search runs"
        );

        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        let first = cached_text_version(&mut h).expect("the jump flattened the buffer");

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(
            cached_text_version(&mut h),
            Some(first),
            "a repeat on an unchanged buffer reuses the same flattening",
        );
    }

    #[test]
    fn an_edit_invalidates_the_flattened_buffer() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        let before = cached_text_version(&mut h).expect("the jump flattened the buffer");

        h.type_keys("i");
        h.type_text("zz");
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);

        let after = cached_text_version(&mut h).expect("the repeat re-flattened");
        assert_ne!(
            after, before,
            "an edit moves the version, so the stale text is not reused",
        );
    }

    /// The reverse walk keeps only two candidates, so pin both the ordinary
    /// pick and the wrap-around against a buffer with matches either side of
    /// the cursor.
    #[test]
    fn find_reverse_picks_the_last_match_before_the_cursor() {
        let regex = super::compile_search_regex("ab").expect("valid regex");
        let text = "ab..ab..ab";

        assert_eq!(
            super::find_reverse(&regex, text, 8),
            Some(4),
            "the nearest match before the cursor wins, not the first or last",
        );
        assert_eq!(super::find_reverse(&regex, text, 9), Some(8));
        assert_eq!(super::find_reverse(&regex, text, 5), Some(4));
        assert_eq!(
            super::find_reverse(&regex, text, 0),
            Some(8),
            "with nothing before the cursor it wraps to the last match",
        );
        assert_eq!(super::find_reverse(&regex, "nope", 2), None);
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
