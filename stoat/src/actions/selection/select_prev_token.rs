//! Select previous token command
//!
//! Finds and selects the previous token from the cursor position, including ALL syntactic tokens
//! such as punctuation, operators, brackets, identifiers, and keywords. This enables
//! low-level backward navigation through code structure.

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the previous token from the current cursor position.
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
    /// - If cursor is mid-token, selects from start of token to cursor
    /// - If no previous token exists, returns None
    /// - Cursor positioned on left/start side of selection
    ///
    /// # Returns
    ///
    /// The byte range of the selected token, or None if no token found.
    ///
    /// # Related
    ///
    /// See also [`crate::actions::selection::select_prev_symbol`] for symbol-level selection
    /// that skips punctuation and operators.
    pub fn select_prev_token(&mut self, cx: &App) -> Option<Range<usize>> {
        use text::ToOffset;

        let buffer_snapshot = self.buffer_snapshot(cx);

        // If there's already a non-empty selection with cursor on right, flip cursor to left
        let current_selection = self.cursor_manager.selection();
        if !current_selection.is_empty() && !current_selection.reversed {
            // Cursor is on the right side, flip it to the left
            let start = current_selection.start;
            let end = current_selection.end;
            let start_offset = buffer_snapshot.point_to_offset(start);
            let end_offset = buffer_snapshot.point_to_offset(end);

            // Flip cursor to the left (start) side
            let selection = crate::cursor::Selection::new(end, start);
            self.cursor_manager.set_selection(selection);

            return Some(start_offset..end_offset);
        }

        let token_snapshot = self.token_snapshot(cx);
        let cursor_pos = self.cursor_manager.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_token: Option<(usize, usize)> = None;

        // Iterate through tokens to find the previous token
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a non-whitespace token
            if token.kind.is_token() {
                // If cursor is strictly inside this token (mid-token), select from start to cursor
                if token_start < cursor_offset && cursor_offset < token_end {
                    prev_token = Some((token_start, cursor_offset));
                    break;
                }

                // Track tokens that end at or before cursor
                if token_end <= cursor_offset {
                    prev_token = Some((token_start, token_end));
                }
            }

            token_cursor.next();
        }

        let found_token = prev_token.map(|(start, end)| start..end);

        // If we found a token, update the cursor and selection
        if let Some(ref range) = found_token {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create reversed selection (cursor on left/start side)
            let selection = crate::cursor::Selection::new(selection_end, selection_start);
            self.cursor_manager.set_selection(selection);
        }

        found_token
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn select_previous_token() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> Result<()>");
        s.set_cursor(0, 22); // At end, after last ">"
        s.command("SelectPrevToken"); // Select last ">" with cursor on left
        s.assert_cursor_notation("fn foo() -> Result<()<||>|>");
        s.command("SelectPrevToken"); // Select ")" with cursor on left
        s.assert_cursor_notation("fn foo() -> Result<(<||)|>>");
    }

    #[test]
    fn select_from_after_token() {
        let mut s = Stoat::test();
        s.set_text("foo  ");
        s.set_cursor(0, 5); // After "foo  " (in trailing space)
        s.command("SelectPrevToken"); // Select "foo" with cursor on left
        s.assert_cursor_notation("<||foo|>  ");
    }

    #[test]
    fn select_punctuation_backward() {
        let mut s = Stoat::test();
        s.set_text("foo.bar");
        s.set_cursor(0, 4); // After the dot (at "b" in "bar")
        s.command("SelectPrevToken"); // Select dot (previous token)
        s.assert_cursor_notation("foo<||.|>bar");
    }

    #[test]
    fn select_operator_backward() {
        let mut s = Stoat::test();
        s.set_text("x + y");
        s.set_cursor(0, 4); // After "x + "
        s.command("SelectPrevToken"); // Select "+" (previous token)
        s.assert_cursor_notation("x <||+|> y");
    }

    #[test]
    fn select_brackets_backward() {
        let mut s = Stoat::test();
        s.set_text("foo()");
        s.set_cursor(0, 5); // After "foo()"
        s.command("SelectPrevToken"); // Select closing paren
        s.assert_cursor_notation("foo(<||)|>");
        s.command("SelectPrevToken"); // Select opening paren
        s.assert_cursor_notation("foo<||(|>)");
    }

    #[test]
    fn skip_spaces_backward() {
        let mut s = Stoat::test();
        s.set_text("42   x");
        s.set_cursor(0, 6); // After "42   x"
        s.command("SelectPrevToken"); // Should select "x" with cursor on left
        s.assert_cursor_notation("42   <||x|>");
    }

    #[test]
    fn skip_newlines_backward() {
        let mut s = Stoat::test();
        s.set_text("foo\n\nx");
        s.set_cursor(1, 0); // Start of line 1 (after first newline)
        s.command("SelectPrevToken"); // Should select "foo" with cursor on left
        s.assert_cursor_notation("<||foo|>\n\nx");
    }

    #[test]
    fn at_start_of_buffer() {
        let mut s = Stoat::test();
        s.set_text("word");
        s.set_cursor(0, 0); // At start
        s.command("SelectPrevToken"); // No token to select
        s.assert_cursor_notation("|word"); // Cursor stays at start
    }

    #[test]
    fn mid_token_selects_from_start() {
        let mut s = Stoat::test();
        s.set_text("foo identifier");
        s.set_cursor(0, 8); // Middle of "identifier" (after "iden")
        s.command("SelectPrevToken"); // Select from start of "identifier" to cursor, cursor on left
        s.assert_cursor_notation("foo <||iden|>tifier");
    }

    #[test]
    fn select_arrow_backward() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> u32");
        s.set_cursor(0, 12); // After "fn foo() -> "
        s.command("SelectPrevToken"); // Select "->"
        s.assert_cursor_notation("fn foo() <||->|> u32");
    }
}
