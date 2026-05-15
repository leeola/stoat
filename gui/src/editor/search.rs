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

#[cfg(test)]
mod tests {
    use super::*;

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
}
