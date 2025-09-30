//! Token-based selection operations
//!
//! This module provides commands for selecting text based on token boundaries,
//! enabling precise selections that align with syntax structure rather than
//! arbitrary character positions.

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the next token from the current cursor position.
    ///
    /// This command skips whitespace and selects the next syntactic token.
    /// Similar to Helix's word selection but token-aware. The selection is
    /// created in visual mode, anchored at the current cursor position.
    ///
    /// # Behavior
    /// - Skips whitespace and newlines to find the next token
    /// - Selects the entire token (respects token boundaries)
    /// - If cursor is mid-token, behavior TBD
    /// - If no next token exists, returns None
    ///
    /// # Returns
    /// The byte range of the selected token, or None if no token found.
    pub fn select_next_token(&self, cx: &App) -> Option<Range<usize>> {
        // TODO: Implement
        let _ = cx;
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::stoat_test::StoatTest;

    /// Test helper: parses "text with |cursor" and returns selected text.
    ///
    /// Uses the marker system to parse cursor position and setup state,
    /// then calls select_next_token and returns the selected text.
    fn sel(input: &str) -> String {
        let mut s = StoatTest::new();
        s.set_text_marked(input);
        s.select_next_token().unwrap_or_default()
    }

    // === Basic token selection ===

    #[test]
    fn select_number_token() {
        assert_eq!(sel("let x = |42"), "42");
    }

    #[test]
    fn select_identifier_at_start() {
        assert_eq!(sel("|identifier"), "identifier");
    }

    #[test]
    fn select_keyword() {
        assert_eq!(sel("keyword |fn"), "fn");
    }

    // === Whitespace skipping ===

    #[test]
    fn skip_spaces() {
        assert_eq!(sel("x |  42"), "42");
    }

    #[test]
    fn skip_newlines() {
        assert_eq!(sel("x|\n\n  foo"), "foo");
    }

    // === Token boundaries ===

    #[test]
    fn select_punctuation_dot() {
        assert_eq!(sel("foo|.bar"), ".");
    }

    #[test]
    fn select_operator() {
        assert_eq!(sel("x |+ y"), "+");
    }

    #[test]
    fn select_open_paren() {
        assert_eq!(sel("fn|()"), "(");
    }

    #[test]
    fn select_open_bracket() {
        assert_eq!(sel("vec|[0]"), "[");
    }

    // === Edge cases ===

    #[test]
    fn at_end_of_buffer() {
        assert_eq!(sel("word|"), "");
    }

    #[test]
    fn mid_token_selects_next() {
        // Decision: mid-token should select the NEXT token, not rest of current
        assert_eq!(sel("id|entifier foo"), "identifier");
    }
}
