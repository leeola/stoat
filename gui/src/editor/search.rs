use crate::editor::{Editor, EditorEvent};
use gpui::Context;
use stoat_text::{Bias, SelectionGoal};

/// Direction the search was opened in. Mirrors the TUI's `/`
/// (forward) and `?` (reverse) openers: forward finds matches at or
/// after the cursor; reverse finds matches before the cursor.
/// `SearchNext` repeats in this direction; `SearchPrev` repeats in
/// the opposite direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Reverse,
}

impl SearchDirection {
    /// Prefix used when rendering the search query in the status bar
    /// or input modal: `/` for forward, `?` for reverse.
    pub fn prefix(self) -> char {
        match self {
            Self::Forward => '/',
            Self::Reverse => '?',
        }
    }

    fn flipped(self) -> Self {
        match self {
            Self::Forward => Self::Reverse,
            Self::Reverse => Self::Forward,
        }
    }
}

/// Persisted in-buffer search state attached to an editor. Carries
/// the most recently submitted query plus the direction it was
/// submitted in. Status-bar and highlight consumers observe the
/// owning editor's `Changed` event and call
/// [`crate::editor::Editor::search_state`] to read the current
/// state.
///
/// Match navigation (`SearchNext` / `SearchPrev`) and highlight
/// painting are sibling work; this slice ships only the storage
/// shape they share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchState {
    query: String,
    direction: SearchDirection,
}

impl SearchState {
    pub fn new(query: impl Into<String>, direction: SearchDirection) -> Self {
        Self {
            query: query.into(),
            direction,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn direction(&self) -> SearchDirection {
        self.direction
    }
}

impl Editor {
    /// Move every selection's primary cursor to the next match of
    /// the stored [`SearchState`]'s query in its stored direction.
    /// Wraps around on no match in the chosen direction. Silent
    /// no-op when no [`SearchState`] is set, the query is empty, or
    /// the query is not a valid regex.
    pub fn search_next(&mut self, cx: &mut Context<'_, Self>) {
        let Some(state) = self.search_state().cloned() else {
            return;
        };
        self.jump_to_match(state.query(), state.direction(), cx);
    }

    /// Move every selection's primary cursor to the next match of
    /// the stored [`SearchState`]'s query in the *opposite* of its
    /// stored direction. Same wrap and no-op semantics as
    /// [`Editor::search_next`].
    pub fn search_prev(&mut self, cx: &mut Context<'_, Self>) {
        let Some(state) = self.search_state().cloned() else {
            return;
        };
        self.jump_to_match(state.query(), state.direction().flipped(), cx);
    }

    fn jump_to_match(
        &mut self,
        query: &str,
        direction: SearchDirection,
        cx: &mut Context<'_, Self>,
    ) {
        if query.is_empty() {
            return;
        }
        let Ok(regex) = stoat::action_handlers::search::compile_search_regex(query) else {
            return;
        };
        let snapshot = self.display_map().update(cx, |dm, _| dm.snapshot());
        let buffer_snapshot = snapshot.buffer_snapshot().clone();
        let text = buffer_snapshot.rope().to_string();
        let head = buffer_snapshot.resolve_anchor(&self.selections().newest_anchor().head());
        let target = match direction {
            SearchDirection::Forward => find_next_match(&regex, &text, head),
            SearchDirection::Reverse => find_prev_match(&regex, &text, head),
        };
        let Some(target) = target else {
            return;
        };
        let anchor = buffer_snapshot.anchor_at(target, Bias::Left);
        self.selections_mut().transform(&buffer_snapshot, |sel| {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

/// Find the first match starting at an offset strictly greater than
/// `head`, wrapping to offset 0 when none exists ahead. Returns the
/// match's start offset, or `None` when the pattern does not match
/// anywhere in `text`.
pub(crate) fn find_next_match(regex: &regex::Regex, text: &str, head: usize) -> Option<usize> {
    let start = head.saturating_add(1).min(text.len());
    if let Some(offset) = next_match_at_or_after(regex, text, start) {
        return Some(offset);
    }
    next_match_at_or_after(regex, text, 0)
}

/// Find the last match whose start is strictly less than `head`,
/// wrapping to the *final* match in `text` when none exists behind.
/// Returns the match's start offset, or `None` when the pattern does
/// not match anywhere in `text`.
pub(crate) fn find_prev_match(regex: &regex::Regex, text: &str, head: usize) -> Option<usize> {
    let starts: Vec<usize> = regex.find_iter(text).map(|m| m.start()).collect();
    if starts.is_empty() {
        return None;
    }
    starts
        .iter()
        .rev()
        .find(|&&pos| pos < head)
        .copied()
        .or_else(|| starts.last().copied())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pattern: &str) -> regex::Regex {
        stoat::action_handlers::search::compile_search_regex(pattern).expect("compile")
    }

    #[test]
    fn new_round_trips_fields() {
        let s = SearchState::new("foo", SearchDirection::Forward);
        assert_eq!(s.query(), "foo");
        assert_eq!(s.direction(), SearchDirection::Forward);
    }

    #[test]
    fn forward_prefix_is_slash() {
        assert_eq!(SearchDirection::Forward.prefix(), '/');
    }

    #[test]
    fn reverse_prefix_is_question_mark() {
        assert_eq!(SearchDirection::Reverse.prefix(), '?');
    }

    #[test]
    fn find_next_match_after_head() {
        assert_eq!(find_next_match(&re("abc"), "abc def abc", 0), Some(8));
    }

    #[test]
    fn find_next_match_wraps_when_none_after() {
        assert_eq!(find_next_match(&re("abc"), "abc def", 5), Some(0));
    }

    #[test]
    fn find_next_match_returns_none_when_no_match() {
        assert_eq!(find_next_match(&re("zzz"), "abc def", 0), None);
    }

    #[test]
    fn find_next_match_regex_pattern() {
        assert_eq!(find_next_match(&re(r"\d+"), "abc 123 def 456", 0), Some(4));
        assert_eq!(find_next_match(&re(r"\d+"), "abc 123 def 456", 7), Some(12));
    }

    #[test]
    fn find_prev_match_before_head() {
        assert_eq!(find_prev_match(&re("abc"), "abc def abc", 10), Some(8));
    }

    #[test]
    fn find_prev_match_wraps_when_none_before() {
        assert_eq!(find_prev_match(&re("abc"), "abc def", 0), Some(0));
    }

    #[test]
    fn find_prev_match_returns_none_when_no_match() {
        assert_eq!(find_prev_match(&re("zzz"), "abc def", 5), None);
    }

    #[test]
    fn find_prev_match_uses_strictly_less_than() {
        assert_eq!(find_prev_match(&re("abc"), "abc def abc", 8), Some(0));
    }
}
