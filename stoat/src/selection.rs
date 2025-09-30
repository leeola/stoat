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
    use crate::Stoat;

    // === Basic token selection ===

    #[test]
    fn select_number_token() {
        let mut s = Stoat::test();
        s.set_text("let x = 42");
        s.set_cursor(0, 8); // Position at start of "42"
        s.input("w"); // Select next token
        s.assert_cursor_notation("let x = <|42||>");
    }

    #[test]
    fn select_identifier_at_start() {
        let mut s = Stoat::test();
        s.set_text("identifier");
        s.input("w"); // Select next token from origin
        s.assert_cursor_notation("<|identifier||>");
    }

    #[test]
    fn select_keyword() {
        let mut s = Stoat::test();
        s.set_text("keyword fn");
        s.set_cursor(0, 8); // After "keyword "
        s.input("w"); // Select next token
        s.assert_cursor_notation("keyword <|fn||>");
    }

    // === Whitespace skipping ===

    #[test]
    fn skip_spaces() {
        let mut s = Stoat::test();
        s.set_text("x   42");
        s.set_cursor(0, 1); // After "x"
        s.input("w"); // Should skip spaces and select "42"
        s.assert_cursor_notation("x   <|42||>");
    }

    #[test]
    fn skip_newlines() {
        let mut s = Stoat::test();
        s.set_text("x\n\n  foo");
        s.set_cursor(0, 1); // After "x"
        s.input("w"); // Should skip newlines/spaces and select "foo"
        s.assert_cursor_notation("x\n\n  <|foo||>");
    }

    // === Token boundaries ===

    #[test]
    fn select_punctuation_dot() {
        let mut s = Stoat::test();
        s.set_text("foo.bar");
        s.set_cursor(0, 3); // After "foo"
        s.input("w"); // Select the "." punctuation
        s.assert_cursor_notation("foo<|.||>bar");
    }

    #[test]
    fn select_operator() {
        let mut s = Stoat::test();
        s.set_text("x + y");
        s.set_cursor(0, 2); // After "x "
        s.input("w"); // Select "+" operator
        s.assert_cursor_notation("x <|+||> y");
    }

    #[test]
    fn select_open_paren() {
        let mut s = Stoat::test();
        s.set_text("fn()");
        s.set_cursor(0, 2); // After "fn"
        s.input("w"); // Select "("
        s.assert_cursor_notation("fn<|(||>)");
    }

    #[test]
    fn select_open_bracket() {
        let mut s = Stoat::test();
        s.set_text("vec[0]");
        s.set_cursor(0, 3); // After "vec"
        s.input("w"); // Select "["
        s.assert_cursor_notation("vec<|[||>0]");
    }

    // === Edge cases ===

    #[test]
    fn at_end_of_buffer() {
        let mut s = Stoat::test();
        s.set_text("word");
        s.set_cursor(0, 4); // At end
        s.input("w"); // No token to select
        s.assert_cursor_notation("word|"); // Cursor stays at end
    }

    #[test]
    fn mid_token_selects_rest() {
        let mut s = Stoat::test();
        s.set_text("identifier foo");
        s.set_cursor(0, 2); // Middle of "identifier"
        s.input("w"); // Select rest of current token
        s.assert_cursor_notation("id<|entifier||> foo");
    }
}
