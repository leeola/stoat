//! Select next token command
//!
//! Finds and selects the next token from the cursor position, including ALL syntactic tokens
//! such as punctuation, operators, brackets, identifiers, and keywords. This enables
//! low-level navigation through code structure.

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the next token from the current cursor position.
    ///
    /// Selects ANY syntactic token including punctuation, operators, brackets,
    /// identifiers, and keywords. The selection is created without changing editor mode.
    ///
    /// # Token Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    /// - Operators: `+`, `-`, `->`, `==`
    /// - Punctuation: `.`, `,`, `;`, `:`
    /// - Brackets: `(`, `)`, `{`, `}`, `[`, `]`
    ///
    /// # Behavior
    ///
    /// - Skips only whitespace and newlines
    /// - Selects the entire token (respects token boundaries)
    /// - If cursor is mid-token, selects remainder of current token
    /// - If no next token exists, returns None
    /// - Cursor positioned on right/end side of selection
    ///
    /// # Returns
    ///
    /// The byte range of the selected token, or None if no token found.
    ///
    /// # Related
    ///
    /// See also [`crate::actions::selection::select_next_symbol`] for symbol-level selection
    /// that skips punctuation and operators.
    pub fn select_next_token(&mut self, cx: &App) -> Option<Range<usize>> {
        use text::ToOffset;

        let buffer_snapshot = self.buffer_snapshot(cx);

        // If there's already a non-empty selection with cursor on left, flip cursor to right
        let current_selection = self.cursor_manager.selection();
        if !current_selection.is_empty() && current_selection.reversed {
            // Cursor is on the left side, flip it to the right
            let start = current_selection.start;
            let end = current_selection.end;
            let start_offset = buffer_snapshot.point_to_offset(start);
            let end_offset = buffer_snapshot.point_to_offset(end);

            // Flip cursor to the right (end) side
            let selection = crate::cursor::Selection::new(start, end);
            self.cursor_manager.set_selection(selection);

            return Some(start_offset..end_offset);
        }

        let token_snapshot = self.token_snapshot(cx);
        let cursor_pos = self.cursor_manager.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_token = None;

        // Iterate through tokens to find the next token
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a non-whitespace token
            if token.kind.is_token() {
                // Select from cursor position to end of token
                let selection_start = cursor_offset.max(token_start);
                found_token = Some(selection_start..token_end);
                break;
            }

            // Not a token (whitespace), keep looking
            token_cursor.next();
        }

        // If we found a token, update the cursor and selection
        if let Some(ref range) = found_token {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create the selection (cursor on right/end side by default)
            let selection = crate::cursor::Selection::new(selection_start, selection_end);
            self.cursor_manager.set_selection(selection);
        }

        found_token
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn select_token() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> Result<()>");
        s.set_cursor(0, 0); // At start
        s.command("SelectNextToken"); // Select "fn"
        s.assert_cursor_notation("<|fn||> foo() -> Result<()>");
        s.command("SelectNextToken"); // Select "foo"
        s.assert_cursor_notation("fn <|foo||>() -> Result<()>");
    }

    #[test]
    fn select_punctuation() {
        let mut s = Stoat::test();
        s.set_text("foo.bar");
        s.set_cursor(0, 3); // At the dot
        s.command("SelectNextToken"); // Select the dot token
        s.assert_cursor_notation("foo<|.||>bar");
    }

    #[test]
    fn select_operator() {
        let mut s = Stoat::test();
        s.set_text("x + y");
        s.set_cursor(0, 2); // At the plus
        s.command("SelectNextToken"); // Select the plus token
        s.assert_cursor_notation("x <|+||> y");
    }

    #[test]
    fn select_brackets() {
        let mut s = Stoat::test();
        s.set_text("foo()");
        s.set_cursor(0, 3); // After "foo", at opening paren
        s.command("SelectNextToken"); // Select opening paren
        s.assert_cursor_notation("foo<|(||>)");
    }

    #[test]
    fn select_identifier_at_start() {
        let mut s = Stoat::test();
        s.set_text("identifier");
        s.command("SelectNextToken"); // Select next token from origin
        s.assert_cursor_notation("<|identifier||>");
    }

    #[test]
    fn skip_spaces() {
        let mut s = Stoat::test();
        s.set_text("x   42");
        s.set_cursor(0, 1); // After "x"
        s.command("SelectNextToken"); // Should skip spaces and select "42"
        s.assert_cursor_notation("x   <|42||>");
    }

    #[test]
    fn skip_newlines() {
        let mut s = Stoat::test();
        s.set_text("x\n\n  foo");
        s.set_cursor(0, 1); // After "x"
        s.command("SelectNextToken"); // Should skip newlines/spaces and select "foo"
        s.assert_cursor_notation("x\n\n  <|foo||>");
    }

    #[test]
    fn at_end_of_buffer() {
        let mut s = Stoat::test();
        s.set_text("word");
        s.set_cursor(0, 4); // At end
        s.command("SelectNextToken"); // No token to select
        s.assert_cursor_notation("word|"); // Cursor stays at end
    }

    #[test]
    fn mid_token_selects_rest() {
        let mut s = Stoat::test();
        s.set_text("identifier foo");
        s.set_cursor(0, 2); // Middle of "identifier"
        s.command("SelectNextToken"); // Select rest of current token
        s.assert_cursor_notation("id<|entifier||> foo");
    }

    #[test]
    fn select_arrow_operator() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> u32");
        s.set_cursor(0, 9); // After "fn foo() ", at "->"
        s.command("SelectNextToken"); // Select "->" token
        s.assert_cursor_notation("fn foo() <|->||> u32");
    }
}
