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
    pub(crate) previous_mode: String,
}

/// Persisted query + direction from the most recent submitted
/// search. `SearchNext` / `SearchPrev` consume this; cleared when
/// the search input is cancelled with empty submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastSearch {
    pub(crate) query: String,
    pub(crate) direction: SearchDirection,
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
    let previous_mode = stoat.mode.clone();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::Search, "", "prompt", 1);
    stoat.search_input = Some(SearchInputState {
        input,
        direction,
        previous_mode,
    });
    stoat.mode = "prompt".into();
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
    let previous_mode = state.previous_mode.clone();
    let direction = state.direction;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;

    if query.is_empty() {
        return true;
    }

    jump_to_match(stoat, &query, direction);
    stoat.last_search = Some(LastSearch { query, direction });
    true
}

/// Cancel the input modal without changing the cursor. Disposes
/// the embedded [`InputView`] and restores the previous mode.
pub(crate) fn search_cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.search_input.take() else {
        return false;
    };
    let previous_mode = state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    stoat.mode = previous_mode;
    true
}

pub(super) fn search_next(stoat: &mut Stoat) -> UpdateEffect {
    let Some(last) = stoat.last_search.clone() else {
        return UpdateEffect::None;
    };
    if jump_to_match(stoat, &last.query, last.direction) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

pub(super) fn search_prev(stoat: &mut Stoat) -> UpdateEffect {
    let Some(last) = stoat.last_search.clone() else {
        return UpdateEffect::None;
    };
    if jump_to_match(stoat, &last.query, last.direction.flipped()) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

/// Find the next regex match of `query` in the focused editor's
/// buffer, starting from the primary cursor and walking in
/// `direction` with wrap-around, then move every selection's primary
/// cursor to the match start. Returns true when a match was found
/// and the cursor moved. Invalid regex is treated as no match.
fn jump_to_match(stoat: &mut Stoat, query: &str, direction: SearchDirection) -> bool {
    use crate::pane::View;
    use stoat_text::{Bias, SelectionGoal};

    let Ok(regex) = stoat_text::compile_search_regex(query) else {
        return false;
    };
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
    let text = rope.to_string();
    let head = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head());
    let len = text.len();

    let target = match direction {
        SearchDirection::Forward => find_forward(&regex, &text, head, len),
        SearchDirection::Reverse => find_reverse(&regex, &text, head),
    };
    let Some(target) = target else { return false };

    let new_buf = buffer_snapshot;
    let anchor = new_buf.anchor_at(target, Bias::Left);
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        new.collapse_to(anchor, SelectionGoal::None);
        new
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

fn find_reverse(regex: &regex::Regex, text: &str, head: usize) -> Option<usize> {
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
