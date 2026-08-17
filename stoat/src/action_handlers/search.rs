use crate::{
    app::{Stoat, UpdateEffect},
    editor_state::{EditorState, ScrollGlide},
    input_view::{InputView, SubmitTarget},
    jumplist::JumpEntry,
    pane::View,
};
use regex_cursor::{engines::meta, regex_automata::util::syntax};
use std::{collections::HashSet, sync::Arc};
use stoat_text::{char_is_word, Anchor, Rope, Selection};

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
///
/// Everything past [`Self::input`] is the origin the preview computes from:
/// where the editor stood when the prompt opened. Each keystroke restores that
/// state and jumps again from it, so a preview never builds on the previous
/// preview's landing.
pub(crate) struct SearchInputState {
    pub(crate) input: InputView,
    pub(crate) direction: SearchDirection,
    /// Whether the submitted match joins the selection set rather than
    /// replacing the primary, decided by the mode the prompt opened in.
    ///
    /// Recorded at open rather than read at submit, so the same keystrokes
    /// behave the same way whether or not the user left select mode while
    /// typing the pattern.
    extend: bool,
    /// The pane editor's selection set when the prompt opened.
    ///
    /// Recomputing every preview from this, rather than from wherever the last
    /// one landed, is what lets backspace walk a query back: the preview for
    /// `fo` is the same whether the user typed up to it or deleted down to it.
    /// It also bounds an extend-mode preview to origin-plus-one range, where
    /// chaining collects a range per keystroke.
    origin_selections: Arc<[Selection<Anchor>]>,
    /// The pane editor's scroll position when the prompt opened, as
    /// `(scroll_row, scroll_offset)`. Restored when the query stops naming a
    /// match, so an emptied prompt glides back to where the search started.
    origin_view: (u32, f32),
    /// The jumplist entry for the origin, captured before the prompt took
    /// focus. `None` when no editor was focused.
    ///
    /// [`search_submit`] pushes this rather than reading the position at submit
    /// time, which by then is the preview's landing rather than the origin
    /// `C-o` must return to.
    origin_jump: Option<JumpEntry>,
    /// The query the live preview last ran for.
    ///
    /// The preview syncs once per frame, so without this every idle frame
    /// redoes the restore-and-jump for a query that has not changed.
    previewed: Option<String>,
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

/// The open search prompt as the status bar paints it.
///
/// The prompt is the only thing on screen that shows the typed query, so it
/// carries the caret position as well as the text. A snapshot rather than a
/// borrow of [`SearchInputState`], because the caret resolves through a mutable
/// workspace and the frame holds that borrow for the whole paint.
pub(crate) struct SearchPrompt {
    /// `/` for a forward search, `?` for a reverse one.
    pub(crate) sigil: char,
    /// The query typed so far.
    pub(crate) text: String,
    /// Caret position as a byte offset into [`Self::text`].
    ///
    /// Always a char boundary in `0..=text.len()`, so a painter splits the text
    /// there with no check of its own. [`prompt_display`] establishes that: the
    /// offset comes from the input's own editor, and the paint path has no way
    /// to recover from a bad one.
    pub(crate) cursor: usize,
}

impl LastSearch {
    pub(crate) fn new(query: String, direction: SearchDirection, smart_case: bool) -> Self {
        Self {
            regex: compile_cursor_regex(&query, smart_case),
            query,
            direction,
        }
    }
}

/// The open search prompt, or `None` when no search input is open.
///
/// Takes `&mut Stoat` because resolving the caret snapshots the input editor's
/// display map, so the frame must call it before it borrows the workspace.
pub(crate) fn prompt_display(stoat: &mut Stoat) -> Option<SearchPrompt> {
    let (sigil, editor_id, text) = {
        let state = stoat.search_input.as_ref()?;
        let sigil = match state.direction {
            SearchDirection::Forward => '/',
            SearchDirection::Reverse => '?',
        };
        (
            sigil,
            state.input.editor_id,
            state.input.text(stoat.active_workspace()),
        )
    };

    let offset = stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .map(|editor| {
            let display_snapshot = editor.display_map.snapshot();
            let buf_snapshot = display_snapshot.buffer_snapshot();
            let head = editor.selections.newest_anchor().head();
            buf_snapshot.resolve_anchor(&head)
        })
        .unwrap_or(text.len());

    let cursor = (0..=offset.min(text.len()))
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0);

    Some(SearchPrompt {
        sigil,
        text,
        cursor,
    })
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
    let extend = stoat.in_select_mode();

    // Captured before InputView::create, which moves focus onto the prompt's
    // own editor. After it, both of these answer for that editor rather than
    // the pane's.
    let origin_jump = super::jump::live_entry(stoat);
    let origin = pane_editor(stoat).map(|editor| {
        (
            editor.selections.shared_anchors(),
            (editor.scroll_row, editor.scroll_offset),
        )
    });

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::Search, "", "insert", 1);
    let (origin_selections, origin_view) = origin.unwrap_or((Arc::from([]), (0, 0.0)));
    stoat.search_input = Some(SearchInputState {
        input,
        direction,
        extend,
        origin_selections,
        origin_view,
        origin_jump,
        previewed: None,
    });
    UpdateEffect::Redraw
}

/// The focused pane's editor, or `None` when that pane shows something else.
///
/// The search paths reach the editor this way rather than through
/// [`super::focused_editor_mut`], because while the prompt is open focus sits
/// on the prompt's own input view, which that one answers with instead.
fn pane_editor(stoat: &mut Stoat) -> Option<&mut EditorState> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Editor(editor_id) = ws.panes.pane(focused).view else {
        return None;
    };
    ws.editors.get_mut(editor_id)
}

/// Put the pane editor back where the open prompt found it.
///
/// Both halves move together on every path that abandons a preview, so they are
/// restored together too. The view goes back as a glide rather than a jump,
/// which [`crate::app::Stoat::tick_scroll_anim`] eases and snaps past its
/// three-viewport bound.
fn restore_origin(stoat: &mut Stoat, selections: Arc<[Selection<Anchor>]>, view: (u32, f32)) {
    let Some(editor) = pane_editor(stoat) else {
        return;
    };
    editor.selections.restore(selections);

    let (row, offset) = view;
    if editor.scroll_row != row {
        editor.scroll_row = row;
        editor.scroll_glide = ScrollGlide::Page;
    } else {
        editor.scroll_offset = offset;
    }
}

/// Bring the open search prompt's live preview up to date with what is typed.
///
/// A no-op when no prompt is open, or when the query has not changed since the
/// last sync. Otherwise the pane editor returns to the origin captured at open
/// and jumps again from there, so the preview shown is always the one that
/// query alone produces.
///
/// The preview is deliberately silent. A query that wraps, matches nothing, or
/// does not compile yet is an ordinary state mid-typing, so none of them raises
/// a status message the way a submit does. A query with no usable match
/// restores the origin instead.
///
/// Runs from `drive_background` rather than the paint, so the preview lands in
/// the same frame as the keystroke that caused it.
pub(crate) fn sync_search_preview(stoat: &mut Stoat) {
    let Some(state) = stoat.search_input.as_ref() else {
        return;
    };
    let query = state.input.text(stoat.active_workspace());
    if state.previewed.as_deref() == Some(query.as_str()) {
        return;
    }

    let state = stoat.search_input.as_mut().expect("prompt still open");
    state.previewed = Some(query.clone());
    let direction = state.direction;
    let extend = state.extend;
    let selections = Arc::clone(&state.origin_selections);
    let view = state.origin_view;

    restore_origin(stoat, selections, view);

    let regex = (!query.is_empty())
        .then(|| compile_cursor_regex(&query, smart_case(stoat)))
        .flatten();
    if let Some(regex) = regex
        && jump_to_match(stoat, &regex, direction, extend).moved()
    {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = pane_editor(stoat) {
            super::view::follow_jump(editor, scrolloff);
        }
    }

    // The frame reads the query straight off the prompt rather than through a
    // display layer, so nothing else reports that the highlighted matches moved.
    stoat.paint_generation += 1;
}

/// Submit the search query and keep where the preview landed.
///
/// Stores [`LastSearch`] for `n` / `N` to repeat. Returns true when the modal
/// was open, so the prompt-submit router short-circuits.
///
/// The jump re-runs from the origin the prompt captured rather than from the
/// preview's landing. Repeated from the origin it finds the same match again,
/// where a search on from the preview skips past it to the next one.
pub(crate) fn search_submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.search_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let direction = state.direction;
    let extend = state.extend;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    restore_origin(stoat, state.origin_selections, state.origin_view);

    if query.is_empty() {
        stoat.paint_generation += 1;
        return true;
    }

    let last = LastSearch::new(query, direction, smart_case(stoat));
    if let Some(regex) = last.regex.as_ref()
        && jump_to_match(stoat, regex, direction, extend).moved()
        && let Some(entry) = state.origin_jump
    {
        super::jump::push_entry(stoat, entry);
    }
    if last.regex.is_none() {
        stoat.set_status(format!("invalid regex: {}", last.query));
    }
    // The query reaches the frame directly rather than through any display
    // layer, so nothing else reports that the highlighted matches moved.
    stoat.paint_generation += 1;
    stoat.last_search = Some(last);
    true
}

/// Cancel the input modal, putting the pane editor back where the prompt found
/// it. Disposes the embedded [`InputView`].
pub(crate) fn search_cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.search_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    restore_origin(stoat, state.origin_selections, state.origin_view);
    stoat.paint_generation += 1;
    true
}

pub(super) fn search_next(stoat: &mut Stoat) -> UpdateEffect {
    repeat_search(stoat, |direction| direction, false)
}

pub(super) fn search_prev(stoat: &mut Stoat) -> UpdateEffect {
    repeat_search(stoat, SearchDirection::flipped, false)
}

pub(super) fn extend_search_next(stoat: &mut Stoat) -> UpdateEffect {
    repeat_search(stoat, |direction| direction, true)
}

pub(super) fn extend_search_prev(stoat: &mut Stoat) -> UpdateEffect {
    repeat_search(stoat, SearchDirection::flipped, true)
}

/// Repeat the stored search once per pending count, walking the direction
/// `resolve` answers for the one the search was submitted in.
///
/// A step that finds nothing ends the walk rather than failing the press, so
/// the steps before it keep their landing. Only a buffer holding no match at
/// all reaches that, since the search wraps.
///
/// The count is taken before the pattern is read, so a press with nothing
/// stored still consumes it. A count of zero clamps to one: an unbound digit
/// becomes a pending count, so a keymap that frees 0 otherwise sends `0 n`
/// here as a walk of no steps.
///
/// The last step's outcome is what reaches the status bar. A repeat is the
/// only search that reports, so a submitted one stays silent about both the
/// wrap and the empty buffer.
fn repeat_search(
    stoat: &mut Stoat,
    resolve: impl Fn(SearchDirection) -> SearchDirection,
    extend: bool,
) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1).max(1);
    // Redraw either way, since a stored pattern that never compiled reports
    // from here. A press with no search at all repaints for nothing, which
    // costs one frame and keeps the branch simple.
    let Some((regex, direction)) = repeat_target(stoat) else {
        return UpdateEffect::Redraw;
    };
    let direction = resolve(direction);

    let mut outcome = SearchOutcome::NoMatch;
    for _ in 0..count {
        outcome = jump_to_match(stoat, &regex, direction, extend);
        if !outcome.moved() {
            break;
        }
    }

    match outcome {
        SearchOutcome::Landed => {},
        SearchOutcome::Wrapped => stoat.set_status("Wrapped around document"),
        SearchOutcome::NoMatch => stoat.set_status("No more matches"),
    }
    UpdateEffect::Redraw
}

/// Set the search pattern to what the selections hold, so `n` walks the other
/// places that text occurs.
///
/// Each selection is escaped and the alternatives are joined with `|`.
/// Duplicates collapse, so selecting the same word twice searches for it once.
///
/// With `detect_word_boundaries`, a selection edge that sits on a word boundary
/// is anchored there with `\b`. Selecting a whole word then finds that word
/// rather than every occurrence inside a longer one, while a selection cut
/// mid-word stays unanchored and matches anywhere.
pub(super) fn search_selection(stoat: &mut Stoat, detect_word_boundaries: bool) -> UpdateEffect {
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    // Dedup keeping the order the selections arrived in. A set alone answers
    // membership but hands back no order, which leaves nothing for a test to
    // pin and no reason for the user to expect one alternative before another.
    let mut seen = HashSet::new();
    let mut alternatives: Vec<String> = Vec::new();
    for sel in editor.selections.all_anchors() {
        let from = buffer_snapshot.resolve_anchor(&sel.start);
        let to = buffer_snapshot.resolve_anchor(&sel.end);

        let prefix = match detect_word_boundaries && is_at_word_start(rope, from) {
            true => "\\b",
            false => "",
        };
        let suffix = match detect_word_boundaries && is_at_word_end(rope, to) {
            true => "\\b",
            false => "",
        };
        let word = regex::escape(&rope.slice(from..to).to_string());

        let alternative = format!("{prefix}{word}{suffix}");
        if seen.insert(alternative.clone()) {
            alternatives.push(alternative);
        }
    }

    let pattern = alternatives.join("|");
    stoat.set_status(format!("search pattern set to '{pattern}'"));
    set_pattern(stoat, pattern);
    UpdateEffect::Redraw
}

/// Make `pattern` what the next repeat searches for.
///
/// For the paths that name a pattern outright rather than reading one from the
/// search prompt. The direction is forward, since nothing about a named pattern
/// says which way to read it.
///
/// Reports nothing of its own. A caller that wants the reader told says so in
/// its own words.
pub(crate) fn set_pattern(stoat: &mut Stoat, pattern: String) {
    let smart_case = smart_case(stoat);
    stoat.last_search = Some(LastSearch::new(
        pattern,
        SearchDirection::Forward,
        smart_case,
    ));
    // The highlight pass reads the stored query straight off the app, so
    // nothing else reports that a different set of matches now lights up.
    stoat.paint_generation += 1;
}

/// Whether a word starts at `index`, which is where a `\b` before the selection
/// means something.
///
/// The rope end starts no word. At offset 0 the character alone decides, since
/// nothing precedes it to break against.
fn is_at_word_start(rope: &Rope, index: usize) -> bool {
    let Some(ch) = rope.chars_at(index).next() else {
        return false;
    };
    let Some(prev) = rope.reversed_chars_at(index).next() else {
        return char_is_word(ch);
    };
    !char_is_word(prev) && char_is_word(ch)
}

/// Whether a word ends at `index`, which is where a `\b` after the selection
/// means something.
///
/// Neither end of the rope ends a word. At offset 0 nothing precedes the
/// boundary, and at the rope end nothing follows it.
fn is_at_word_end(rope: &Rope, index: usize) -> bool {
    let Some(ch) = rope.chars_at(index).next() else {
        return false;
    };
    let Some(prev) = rope.reversed_chars_at(index).next() else {
        return false;
    };
    char_is_word(prev) && !char_is_word(ch)
}

/// The pattern and direction a repeat press searches with, or `None` when
/// nothing has been searched for or the stored pattern never compiled.
///
/// Reports the stored pattern that never compiled, and stays silent when no
/// search has happened at all. The user typed a pattern in the first case and
/// earned an answer. The second names nothing that failed.
///
/// Clones the regex, which is a refcount bump, rather than the whole
/// [`LastSearch`], which copies the query text on every press. The query is
/// copied on the failure arm alone, which is not a press being repeated.
fn repeat_target(stoat: &mut Stoat) -> Option<(CursorRegex, SearchDirection)> {
    let last = stoat.last_search.as_ref()?;
    if let Some(regex) = last.regex.clone() {
        return Some((regex, last.direction));
    }

    let query = last.query.clone();
    stoat.set_status(format!("invalid regex: {query}"));
    None
}

/// How a search step ended, which is what `n` and `N` report and a submitted
/// search stays silent about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SearchOutcome {
    /// A match lay ahead of the selection, so there is nothing to say.
    Landed,
    /// Nothing lay ahead, so the scan resumed from the buffer's other end.
    Wrapped,
    /// The buffer holds no match at all.
    NoMatch,
}

impl SearchOutcome {
    /// Whether the step landed on a match, wrapping to reach it or not.
    fn moved(self) -> bool {
        !matches!(self, SearchOutcome::NoMatch)
    }
}

/// Find the next match of `regex` in the focused editor's buffer, walking in
/// `direction` with wrap-around, and give it to the primary selection as its
/// new range.
///
/// The whole match becomes the selection, so the operation after a search acts
/// on the matched text. The primary's direction carries over, and every other
/// selection is left alone.
///
/// With `extend`, the match joins the set as an additional range instead, so
/// select mode collects every match walked through rather than moving one
/// selection over them. The added range takes the primary, so the next press
/// walks on from it.
///
/// The search starts at the edge of the primary range the walk moves away from,
/// its end going forward and its start going back. A start taken from the
/// cursor instead puts a reverse repeat inside the match it just landed on,
/// which finds that same match again.
///
/// Takes the pattern already compiled, since `n` and `N` repeat one that
/// [`LastSearch`] built when it was submitted.
fn jump_to_match(
    stoat: &mut Stoat,
    regex: &CursorRegex,
    direction: SearchDirection,
    extend: bool,
) -> SearchOutcome {
    let Some(editor) = pane_editor(stoat) else {
        return SearchOutcome::NoMatch;
    };
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor();
    let reversed = sel.reversed;
    let from = match direction {
        SearchDirection::Forward => buffer_snapshot.resolve_anchor(&sel.end),
        SearchDirection::Reverse => buffer_snapshot.resolve_anchor(&sel.start),
    };

    let target = match direction {
        SearchDirection::Forward => find_forward(regex, rope, from),
        SearchDirection::Reverse => find_reverse(regex, rope, from),
    };
    let Some(hit) = target else {
        return SearchOutcome::NoMatch;
    };
    // An empty match at the very start of the buffer selects nothing and lands
    // nowhere, so there is no jump to make.
    if hit.end == 0 {
        return SearchOutcome::NoMatch;
    }

    match extend {
        true => editor
            .selections
            .add_range(hit.start..hit.end, reversed, buffer_snapshot),
        false => editor
            .selections
            .replace_primary(hit.start..hit.end, reversed, buffer_snapshot),
    }
    match hit.wrapped {
        true => SearchOutcome::Wrapped,
        false => SearchOutcome::Landed,
    }
}

/// A match a search step reached, and whether the scan ran off the end of the
/// buffer and resumed from the other side to get there.
///
/// The wrap travels with the span because only the scan that did it knows it
/// happened, and `n` reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Hit {
    start: usize,
    end: usize,
    wrapped: bool,
}

/// The first match at or after `from`, wrapping to the start of the buffer when
/// nothing lies ahead.
///
/// `from` is the primary selection's end, so the match it currently covers is
/// already behind the bound and a repeat advances.
fn find_forward(regex: &CursorRegex, rope: &Rope, from: usize) -> Option<Hit> {
    if let Some(hit) = next_match_at_or_after(regex, rope, from.min(rope.len())) {
        return Some(hit);
    }
    next_match_at_or_after(regex, rope, 0).map(|hit| Hit {
        wrapped: true,
        ..hit
    })
}

/// The last match starting before `from`, or the last in the file when there is
/// none, so a reverse search wraps around.
///
/// One forward pass, stopping at the first match the cursor has already passed.
/// `find_iter` is lazy, so an ordinary reverse search reads `from` bytes rather
/// than the file. Only a search with nothing behind the cursor, which is the
/// one that has to wrap, reads to the end.
fn find_reverse(regex: &CursorRegex, rope: &Rope, from: usize) -> Option<Hit> {
    let mut matches = regex.find_iter(rope.regex_input(0..rope.len()));
    let mut before = None;

    let at_or_after = loop {
        match matches.next() {
            Some(m) if m.start() < from => {
                before = Some(Hit {
                    start: m.start(),
                    end: m.end(),
                    wrapped: false,
                })
            },
            other => break other,
        }
    };

    if before.is_some() {
        return before;
    }
    let first = at_or_after?;
    let last = matches.last();
    Some(Hit {
        start: last.map_or(first.start(), |m| m.start()),
        end: last.map_or(first.end(), |m| m.end()),
        wrapped: true,
    })
}

/// The first regex match whose start is at or after `at`.
///
/// The scan is bounded to `at..` on the input rather than by slicing, so the
/// offsets it reports are buffer offsets. A zero-width pattern still reports a
/// match before the bound, which is why the start is checked.
///
/// The hit is never marked as wrapped. Only the caller knows whether this scan
/// is the first one or the one that resumed from the buffer's start.
fn next_match_at_or_after(regex: &CursorRegex, rope: &Rope, at: usize) -> Option<Hit> {
    if at > rope.len() {
        return None;
    }
    let m = regex.find(rope.regex_input(at..rope.len()))?;
    if m.start() >= at {
        Some(Hit {
            start: m.start(),
            end: m.end(),
            wrapped: false,
        })
    } else {
        None
    }
}

/// The smart-case setting, defaulting to enabled.
pub(crate) fn smart_case(stoat: &Stoat) -> bool {
    stoat.settings.search_smart_case.unwrap_or(true)
}

/// Whether `pattern` matches any case, given the smart-case setting.
///
/// A pattern typed without any uppercase character carries no evidence the case
/// matters, and lowercase is what a hurried search looks like. One uppercase
/// character is a deliberate reach for shift, so the whole pattern turns
/// case-sensitive.
///
/// The rule lives here rather than at each call site so both compile paths
/// answer the same way, and the jump and the highlight agree about what matched.
fn case_insensitive(pattern: &str, smart_case: bool) -> bool {
    smart_case && !pattern.chars().any(char::is_uppercase)
}

/// Compile `pattern` into a [`regex::Regex`] with multiline mode on,
/// so `^` and `$` match line boundaries inside the buffer text.
pub(crate) fn compile_search_regex(
    pattern: &str,
    smart_case: bool,
) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .multi_line(true)
        .case_insensitive(case_insensitive(pattern, smart_case))
        .build()
}

/// Compile `pattern` for the engine that matches over a rope's chunks, with the
/// same multiline and case modes [`compile_search_regex`] uses.
///
/// The two exist side by side because they suit different haystacks. This one
/// searches the buffer without flattening it. The plain one is what the
/// highlight pass runs over the window it has already built as a string.
pub(crate) fn compile_cursor_regex(pattern: &str, smart_case: bool) -> Option<CursorRegex> {
    CursorRegex::builder()
        .syntax(
            syntax::Config::new()
                .multi_line(true)
                .case_insensitive(case_insensitive(pattern, smart_case)),
        )
        .build(pattern)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::SearchDirection;
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

    /// A match the scan reached without wrapping.
    fn hit(start: usize, end: usize) -> Option<super::Hit> {
        Some(super::Hit {
            start,
            end,
            wrapped: false,
        })
    }

    /// A match the scan reached only by running off one end of the buffer and
    /// resuming from the other.
    fn wrapped_hit(start: usize, end: usize) -> Option<super::Hit> {
        Some(super::Hit {
            start,
            end,
            wrapped: true,
        })
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

    /// The match becomes the selection, not a cursor parked at its start, so
    /// the operation after a search acts on the matched text.
    #[test]
    fn forward_search_selects_the_first_match_after_the_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenSearchInput);
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
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
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
    }

    /// The goto menu's select flavor reaches reverse search, and the prompt
    /// reads its origin through `in_select_mode`, which answers yes for the
    /// `select` the chord hands back before it opens the prompt.
    #[test]
    fn select_goto_question_opens_a_reverse_search_that_extends() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("v l l");

        h.type_keys("g");
        assert_eq!(h.stoat.focused_mode(), "select_goto", "menu entered");

        h.type_keys("?");
        let state = h.stoat.search_input.as_ref().expect("prompt opened");
        assert_eq!(state.direction, SearchDirection::Reverse);
        assert!(state.extend, "opened from select mode, so the match joins");
    }

    /// A `?` chord opens a prompt and hands the menu's mode back, and the order
    /// decides which editor the hand-back lands on. After the open, focus is the
    /// prompt's own editor, so a switch there drops the prompt out of insert.
    ///
    /// The wrong order has two symptoms, and both are pinned here. A prompt
    /// that takes no keys is indistinguishable from one that never opened. A
    /// pane left in the menu mode surfaces the moment the prompt closes.
    #[test]
    fn a_reverse_search_chord_takes_keys_and_hands_the_mode_back() {
        for (chord, back) in [("g ?", "normal"), ("v g ?", "select")] {
            let mut h = TestHarness::with_size(40, 10);
            seed(&mut h, "abc def abc\n");

            h.type_keys(chord);
            h.type_text("de");

            let state = h.stoat.search_input.as_ref().expect("prompt opened");
            assert_eq!(
                state.input.text(h.stoat.active_workspace()),
                "de",
                "`{chord}` opens a prompt that takes the keys typed into it",
            );
            assert_eq!(h.stoat.focused_mode(), "insert", "`{chord}` prompt is live");

            h.type_keys("escape");
            assert_eq!(
                h.stoat.focused_mode(),
                back,
                "`{chord}` leaves the menu behind once the prompt closes",
            );
        }
    }

    #[test]
    /// The help binding covers every mode that does not claim `?`, and it
    /// outranks a mode block on atom count, so this says the normal goto menu
    /// still reaches reverse search now that `select_goto` has joined its
    /// exclusion list.
    fn goto_question_opens_a_reverse_search() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");

        h.type_keys("g ?");
        let state = h.stoat.search_input.as_ref().expect("prompt opened");
        assert_eq!(state.direction, SearchDirection::Reverse);
        assert!(
            !state.extend,
            "opened from normal mode, so the match replaces"
        );
    }

    #[test]
    fn reverse_search_selects_the_first_match_before_the_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("l l l l l l l l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenReverseSearchInput);
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
    }

    #[test]
    fn reverse_search_wraps_when_no_match_before_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenReverseSearchInput);
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
    }

    /// A variable-length pattern selects what it actually matched, which is
    /// what a fixed-width landing at the match start hides.
    #[test]
    fn a_variable_length_match_selects_its_whole_span() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "ab 1 22 333\n");
        h.type_keys("/");
        h.type_text("\\d+");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(3, 4, false)]);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(5, 7, false)]);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
    }

    /// A reversed primary keeps facing backward through a search, so the set
    /// does not silently flip under the user.
    #[test]
    fn a_search_keeps_the_primary_facing_the_way_it_did() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::FlipSelections);
        assert!(h.selection_spans()[0].2, "the fixture starts reversed");

        h.type_keys("escape");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, true)]);
    }

    /// Only the primary takes the match. Landing it on every selection makes
    /// the spans identical, and identical spans merge, which collapses the set
    /// to one on the first press.
    #[test]
    fn a_search_leaves_the_other_selections_alone() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        h.type_keys("/");
        h.type_text("def");
        h.type_keys("enter");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false), (4, 7, false)],
            "the non-primary keeps its own span while the primary takes the match",
        );
    }

    /// A repeat that ran off the end of the buffer says so, and one that found
    /// a match ahead of the cursor says nothing.
    ///
    /// Without the report the jump back to the top is indistinguishable from
    /// an ordinary step, so a walk through a file silently starts over.
    #[test]
    fn a_repeat_that_wraps_reports_it() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
        assert_eq!(
            h.stoat.pending_message, None,
            "the submitted search reports nothing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("Wrapped around document"),
        );

        h.stoat.pending_message = None;
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
        assert_eq!(
            h.stoat.pending_message, None,
            "a match ahead of the cursor is not worth reporting",
        );
    }

    /// A repeat for a pattern the buffer does not hold says so rather than
    /// looking like a dead key.
    #[test]
    fn a_repeat_with_no_match_anywhere_reports_it() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def\n");
        h.type_keys("/");
        h.type_text("zzz");
        h.type_keys("enter");
        assert_eq!(
            h.stoat.pending_message, None,
            "the submitted search reports nothing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.stoat.pending_message.as_deref(), Some("No more matches"),);
    }

    /// In select mode a repeat collects, so the set grows by one per press and
    /// every earlier range stays.
    ///
    /// Moving one selection over the matches instead loses everything walked
    /// past, which is the point of searching from select mode at all.
    #[test]
    fn a_select_mode_repeat_adds_the_match_to_the_set() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);

        h.type_keys("v");
        h.type_keys("n");
        assert_eq!(
            h.selection_spans(),
            vec![(8, 11, false), (16, 19, false)],
            "the match joined the set rather than replacing it",
        );

        h.type_keys("n");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 3, false), (8, 11, false), (16, 19, false)],
            "and the walk carried on from the one just added, wrapping",
        );
    }

    /// A normal-mode repeat still moves one selection rather than collecting.
    #[test]
    fn a_normal_mode_repeat_still_replaces_the_primary() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(16, 19, false)]);
    }

    /// A prompt opened in select mode appends at submit, whatever mode the
    /// user is in by the time they press enter.
    ///
    /// Reading the mode at submit instead makes the same keystrokes behave
    /// differently depending on when the pattern was finished.
    #[test]
    fn a_select_mode_prompt_appends_at_submit() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("v l l");
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);

        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 3, false), (8, 11, false)],
            "the original selection survives beside the match",
        );
    }

    /// A lowercase pattern carries no evidence the case matters, so it finds
    /// text in any case.
    #[test]
    fn a_lowercase_pattern_matches_any_case() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xx ABC\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(3, 6, false)]);
    }

    /// One uppercase character is a deliberate reach for shift, so the whole
    /// pattern turns case-sensitive.
    #[test]
    fn an_uppercase_character_makes_the_pattern_case_sensitive() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xx abc\n");
        h.type_keys("/");
        h.type_text("Abc");
        h.type_keys("enter");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false)],
            "nothing matched, so the cursor stayed where it was",
        );
    }

    /// Turning the setting off searches case-sensitively whatever the pattern
    /// looks like.
    #[test]
    fn smart_case_off_keeps_a_lowercase_pattern_case_sensitive() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xx ABC\n");
        h.stoat.settings.search_smart_case = Some(false);
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(0, 1, false)]);
    }

    /// The stored pattern, or `None` when nothing has been searched for.
    fn stored_pattern(h: &TestHarness) -> Option<&str> {
        h.stoat.last_search.as_ref().map(|s| s.query.as_str())
    }

    fn select_ranges(h: &mut TestHarness, ranges: &[(usize, usize)]) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let pieces = ranges.to_vec();
        editor
            .selections
            .split_each(buf_snap, stoat_text::Bias::Right, |_| pieces.clone());
    }

    /// A selection covering a whole word is anchored at both edges, so the
    /// pattern finds that word rather than every occurrence inside a longer
    /// one.
    #[test]
    fn a_word_selection_gets_boundary_anchors() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "foo foobar\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &action::SearchSelectionDetectWordBoundaries,
        );
        assert_eq!(stored_pattern(&h), Some("\\bfoo\\b"));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("search pattern set to '\\bfoo\\b'"),
        );
    }

    /// A selection cut mid-word is anchored at neither edge, since no word
    /// boundary sits there to anchor to.
    #[test]
    fn a_mid_word_selection_gets_no_anchors() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "foobar\n");
        h.type_keys("l");
        h.type_keys("v l");
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &action::SearchSelectionDetectWordBoundaries,
        );
        assert_eq!(stored_pattern(&h), Some("oo"));
    }

    /// The plain form never anchors, whatever the selection's edges sit on.
    #[test]
    fn the_plain_form_never_anchors() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "foo foobar\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchSelection);
        assert_eq!(stored_pattern(&h), Some("foo"));
    }

    /// Distinct selections join as alternatives, and identical ones collapse,
    /// so selecting the same word twice searches for it once.
    #[test]
    fn selections_join_as_alternatives_and_duplicates_collapse() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "ab cd ab\n");
        select_ranges(&mut h, &[(0, 2), (3, 5), (6, 8)]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchSelection);
        assert_eq!(stored_pattern(&h), Some("ab|cd"));
    }

    /// A selection holding regex syntax is escaped, so it searches for the
    /// text rather than compiling as a pattern of its own.
    #[test]
    fn a_selection_holding_regex_syntax_is_escaped() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "a.c\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchSelection);
        assert_eq!(stored_pattern(&h), Some("a\\.c"));

        // The stored pattern drives n, so it has to find the literal text.
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
    }

    /// A count walks that many matches in one press, rather than one.
    #[test]
    fn a_count_repeats_the_search_that_many_times() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc x abc y abc z abc w abc\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(6, 9, false)]);

        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(
            h.selection_spans(),
            vec![(24, 27, false)],
            "three matches on, not one",
        );

        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(
            h.selection_spans(),
            vec![(6, 9, false)],
            "and the same three back",
        );
    }

    #[test]
    fn search_next_repeats_forward_search() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext);
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
    }

    /// The text the primary selection covers, which says whether a jump landed
    /// on a match rather than where a match used to be.
    fn selected_text(h: &mut TestHarness) -> String {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        buf_snap.rope().slice(start..end).to_string()
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

        assert_eq!(
            selected_text(&mut h),
            "abc",
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
        let regex = super::compile_cursor_regex("\u{FC}ber", false).expect("valid regex");

        // A selection covering the umlaut at 0 ends at 2, so the scan starts
        // there. The bound lands mid-character whenever a caller hands over an
        // offset the buffer clipped differently.
        assert_eq!(
            super::find_forward(&regex, &text, 1),
            hit(6, 11),
            "a start inside the umlaut still reaches the next match",
        );
        assert_eq!(
            super::find_forward(&regex, &text, 11),
            wrapped_hit(0, 5),
            "and with nothing after it, wraps rather than stalling there",
        );

        // Four bytes wide, so a start one byte in falls inside the character.
        let wide = stoat_text::Rope::from("\u{1F600}x\u{1F600}x");
        let regex = super::compile_cursor_regex("x", false).expect("valid regex");
        assert_eq!(super::find_forward(&regex, &wide, 1), hit(4, 5));
        assert_eq!(super::find_forward(&regex, &wide, 6), hit(9, 10));
    }

    /// Case-insensitive search folds non-ASCII pairs, not just ASCII ones.
    #[test]
    fn find_forward_folds_non_ascii_case() {
        let text = stoat_text::Rope::from("a\u{FC}b");
        let regex = super::compile_cursor_regex("(?i)\u{DC}", false).expect("valid regex");
        assert_eq!(
            super::find_forward(&regex, &text, 0),
            hit(1, 3),
            "an uppercase umlaut pattern matches the lowercase one",
        );

        let sensitive = super::compile_cursor_regex("\u{DC}", false).expect("valid regex");
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
        let regex = super::compile_cursor_regex("", false).expect("valid regex");
        assert_eq!(
            super::find_forward(&regex, &text, 1),
            hit(2, 2),
            "the boundary after the umlaut, not the byte inside it",
        );
    }

    /// The reverse walk keeps only two candidates, so pin both the ordinary
    /// pick and the wrap-around against a buffer with matches either side of
    /// the cursor.
    #[test]
    fn find_reverse_picks_the_last_match_before_the_cursor() {
        let regex = super::compile_cursor_regex("ab", false).expect("valid regex");
        let text = stoat_text::Rope::from("ab..ab..ab");

        assert_eq!(
            super::find_reverse(&regex, &text, 8),
            hit(4, 6),
            "the nearest match before the cursor wins, not the first or last",
        );
        assert_eq!(super::find_reverse(&regex, &text, 9), hit(8, 10));
        assert_eq!(super::find_reverse(&regex, &text, 5), hit(4, 6));
        assert_eq!(
            super::find_reverse(&regex, &text, 0),
            wrapped_hit(8, 10),
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
        let needle = "needle_in_a_haystack";
        let regex = super::compile_cursor_regex(needle, false).expect("valid regex");
        let target = at + "the ".len();
        let end = target + needle.len();

        assert!(
            rope.chunks().count() > 1,
            "the fixture has to span chunks for this to test anything"
        );
        assert_eq!(
            super::find_forward(&regex, &rope, 0),
            hit(target, end),
            "found walking forward from the start"
        );
        assert_eq!(
            super::find_reverse(&regex, &rope, rope.len()),
            hit(target, end),
            "and walking back from the end"
        );
        assert_eq!(
            super::find_forward(&regex, &rope, end),
            wrapped_hit(target, end),
            "and wrapping round when the selection covers it"
        );
    }

    #[test]
    fn search_prev_flips_direction() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc xyz\n");
        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(
            h.selection_spans(),
            vec![(0, 3, false)],
            "the reverse walk starts at the selection's start, so it leaves the match it is on",
        );
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
        assert_eq!(h.selection_spans(), vec![(8, 11, false)]);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(
            h.selection_spans(),
            vec![(0, 3, false)],
            "back to the first match",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &action::SearchPrev);
        assert_eq!(
            h.selection_spans(),
            vec![(16, 19, false)],
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
            h.stoat.pending_message.as_deref(),
            Some("invalid regex: ["),
            "the submit says why nothing happened",
        );

        h.stoat.pending_message = None;
        assert_eq!(
            crate::action_handlers::dispatch(&mut h.stoat, &action::SearchNext),
            crate::app::UpdateEffect::Redraw,
            "and the repeat repaints to say it again",
        );
        assert_eq!(h.stoat.pending_message.as_deref(), Some("invalid regex: ["),);
    }

    /// Every query change moves the paint generation, and moving the cursor
    /// does not.
    ///
    /// The query is read straight off the app when a frame is built, so no
    /// display layer reports that the highlighted matches changed. That holds
    /// for each previewed query as much as for the submit. A motion changes
    /// what is selected, which the display map already answers for.
    #[test]
    fn every_query_change_moves_the_paint_generation() {
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
        let previewed = h.stoat.paint_generation;
        assert!(
            previewed > before,
            "a previewed query changes which matches are highlighted",
        );

        h.type_keys("enter");
        assert_eq!(
            h.stoat.paint_generation,
            previewed + 1,
            "and the submit that keeps the landing moves it once more",
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

    /// The pane editor's selection ranges, as `(start, end)` byte offsets.
    ///
    /// [`cursor_offset`] and `TestHarness::selection_spans` both read the
    /// *focused* editor, which is no use here. While the prompt is open that is
    /// the prompt's own input view, not the pane the preview moves.
    fn pane_spans(h: &mut TestHarness) -> Vec<(usize, usize)> {
        let editor = super::pane_editor(&mut h.stoat).expect("pane holds an editor");
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

    fn pane_scroll_row(h: &mut TestHarness) -> u32 {
        super::pane_editor(&mut h.stoat)
            .expect("pane holds an editor")
            .scroll_row
    }

    /// Type into the open prompt and run the preview a frame runs.
    ///
    /// The tests drive the sync directly rather than through a paint, the way
    /// the file-finder preview tests do, so what a keystroke previews is
    /// asserted without a render in the middle.
    fn preview(h: &mut TestHarness, text: &str) {
        h.type_text(text);
        super::sync_search_preview(&mut h.stoat);
    }

    /// The editor sits on the match before Enter, which is the whole point of
    /// the preview. The user judges the query by what they see.
    #[test]
    fn typing_a_query_previews_the_first_match() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("/");

        preview(&mut h, "def");
        assert_eq!(
            pane_spans(&mut h),
            [(4, 7)],
            "the preview sits on the match"
        );
        assert!(h.stoat.last_search.is_none(), "nothing is stored yet");
    }

    /// Every preview recomputes from the origin, never from where the previous
    /// one landed.
    ///
    /// Chained previews walk the cursor forward one match per keystroke. A
    /// query typed left to right then lands somewhere different from the same
    /// query pasted whole, and backspace has nothing to walk back to.
    #[test]
    fn each_keystroke_previews_from_the_origin_not_the_last_preview() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "aa ab aa ab\n");
        h.type_keys("/");

        preview(&mut h, "a");
        assert_eq!(
            pane_spans(&mut h),
            [(1, 2)],
            "the first `a` after the cursor"
        );

        preview(&mut h, "b");
        assert_eq!(
            pane_spans(&mut h),
            [(3, 5)],
            "`ab` is found from the origin, not from the previous landing",
        );

        h.type_keys("backspace");
        super::sync_search_preview(&mut h.stoat);
        assert_eq!(
            pane_spans(&mut h),
            [(1, 2)],
            "and backspace walks the preview back to what `a` alone previews",
        );
    }

    /// A pattern mid-typing is routinely not yet valid, so it restores rather
    /// than erroring. A report instead flashes a message on the way to every
    /// bracketed pattern.
    #[test]
    fn a_partial_pattern_restores_the_origin_without_a_message() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        let origin = pane_spans(&mut h);
        h.type_keys("/");

        preview(&mut h, "fo[");
        assert_eq!(pane_spans(&mut h), origin, "an uncompilable query restores");
        assert!(
            h.stoat.pending_message.is_none(),
            "and the preview stays silent",
        );
    }

    /// Emptying the prompt is the user backing out of the query, so the editor
    /// goes back to where the search started rather than holding the last match
    /// that happened to compile.
    #[test]
    fn deleting_the_query_restores_the_origin() {
        let mut h = TestHarness::with_size(20, 6);
        seed(&mut h, "one\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\ntarget\n");
        let origin = pane_spans(&mut h);
        h.type_keys("/");

        preview(&mut h, "target");
        assert_ne!(
            pane_spans(&mut h),
            origin,
            "the preview moved off the origin"
        );
        let previewed_row = pane_scroll_row(&mut h);

        for _ in 0.."target".len() {
            h.type_keys("backspace");
        }
        super::sync_search_preview(&mut h.stoat);

        assert_eq!(pane_spans(&mut h), origin, "the selection came back");
        assert_ne!(previewed_row, 0, "the preview had scrolled away");
        assert_eq!(pane_scroll_row(&mut h), 0, "and the view came back too");
    }

    /// Escape abandons the search, so it undoes the preview as well as closing
    /// the prompt.
    #[test]
    fn escape_after_a_preview_restores_the_origin() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        let origin = pane_spans(&mut h);
        h.type_keys("/");

        preview(&mut h, "def");
        assert_ne!(pane_spans(&mut h), origin, "the preview moved");

        h.type_keys("escape");
        assert_eq!(pane_spans(&mut h), origin, "escape puts it back");
        assert!(h.stoat.last_search.is_none(), "and stores no search");
    }

    /// Enter keeps the preview's landing, and `C-o` returns to where the search
    /// started rather than to the preview.
    #[test]
    fn enter_keeps_the_preview_and_jumps_back_to_the_origin() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        let origin = pane_spans(&mut h);
        h.type_keys("/");

        preview(&mut h, "def");
        h.type_keys("enter");
        assert_eq!(pane_spans(&mut h), [(4, 7)], "the landing survives submit");
        assert_eq!(
            h.stoat.last_search.as_ref().map(|last| last.query.as_str()),
            Some("def"),
            "and the query is stored for n / N",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &action::JumpBackward);
        assert_eq!(pane_spans(&mut h), origin, "C-o returns to the origin");
    }

    /// An extend-mode preview holds origin-plus-one range however many
    /// keystrokes it took to type the query. The recompute from the origin is
    /// what bounds it. Chained previews add a range per keystroke instead.
    #[test]
    fn a_select_mode_preview_adds_one_range_per_query_not_per_keystroke() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc def abc\n");
        h.type_keys("v l l");
        let origin = pane_spans(&mut h);
        h.type_keys("/");

        preview(&mut h, "d");
        preview(&mut h, "e");
        preview(&mut h, "f");

        let spans = pane_spans(&mut h);
        assert_eq!(spans.len(), origin.len() + 1, "one range joined the set");
        assert_eq!(spans[0], origin[0], "the origin range is untouched");
        assert_eq!(spans[1], (4, 7), "and the match is the one that joined");
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
        assert_eq!(h.selection_spans(), vec![(4, 7, false)]);
    }

    #[test]
    fn regex_anchors_match_only_at_line_start() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xfoo\nfoo bar\n");
        h.type_keys("/");
        h.type_text("^foo");
        h.type_keys("enter");
        assert_eq!(h.selection_spans(), vec![(5, 8, false)]);
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

    /// The painted matches follow the setting, not just the query text.
    ///
    /// Smart case decides what the query compiles to, and the cache carries its
    /// compiled regex across rebuilds. A config reload moves the setting while
    /// the query and the buffer hold still, which is the one way the two get
    /// out of step. The jump path recompiles per press, so a cache that misses
    /// the flip paints matches the next n never lands on.
    #[test]
    fn search_match_cache_recomputes_when_smart_case_flips() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc ABC\n");

        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(
            cached_match_count(&mut h),
            2,
            "a lowercase query under smart case matches both spellings",
        );

        h.stoat.settings.search_smart_case = Some(false);
        let _ = h.render_composited();
        assert_eq!(
            cached_match_count(&mut h),
            1,
            "turning smart case off leaves only the exact-case match",
        );
    }
}
