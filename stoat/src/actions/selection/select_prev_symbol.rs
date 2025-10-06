//! Select previous symbol command
//!
//! Finds and selects the previous symbol (identifier, keyword, or literal) from the cursor
//! position, automatically skipping whitespace, punctuation, and operators. This enables semantic
//! backward navigation through code by jumping between meaningful named entities.

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the previous symbol from the current cursor position.
    ///
    /// Skips whitespace, punctuation, and operators to find the previous alphanumeric token
    /// (identifier, keyword, or number). The selection is created without changing editor mode.
    ///
    /// # Symbol Types
    ///
    /// Selects any of:
    /// - Identifiers: `foo`, `bar_baz`, `MyType`
    /// - Keywords: `fn`, `let`, `struct`
    /// - Numbers: `42`, `3.14`
    ///
    /// # Behavior
    ///
    /// - Skips whitespace, newlines, punctuation, and operators
    /// - Selects the entire symbol (respects token boundaries)
    /// - If cursor is mid-symbol, selects from start of symbol to cursor
    /// - If no previous symbol exists, returns None
    ///
    /// # Returns
    ///
    /// The byte range of the selected symbol, or None if no symbol found.
    ///
    /// # Related
    ///
    /// See also [`select_next_symbol`](crate::actions::selection::select_next_symbol) for
    /// forward symbol selection.
    pub fn select_prev_symbol(&mut self, cx: &App) -> Option<Range<usize>> {
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

        let mut prev_symbol: Option<(usize, usize)> = None;

        // Iterate through tokens to find the previous symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If cursor is strictly inside this token (mid-token), select from start to cursor
                if token_start < cursor_offset && cursor_offset < token_end {
                    prev_symbol = Some((token_start, cursor_offset));
                    break;
                }

                // Track symbols that end strictly before cursor
                if token_end < cursor_offset {
                    prev_symbol = Some((token_start, token_end));
                }
            }

            token_cursor.next();
        }

        let found_symbol = prev_symbol.map(|(start, end)| start..end);

        // If we found a symbol, update the cursor and selection
        if let Some(ref range) = found_symbol {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create reversed selection (cursor on left/start side)
            let selection = crate::cursor::Selection::new(selection_end, selection_start);
            self.cursor_manager.set_selection(selection);
        }

        found_symbol
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn select_previous_symbol() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> Result<()>");
        s.set_cursor(0, 22); // At end
        s.command("SelectPrevSymbol"); // Select "Result" with cursor on left
        s.assert_cursor_notation("fn foo() -> <||Result|><()>");
        s.command("SelectPrevSymbol"); // Select "foo" with cursor on left
        s.assert_cursor_notation("fn <||foo|>() -> Result<()>");
    }

    #[test]
    fn select_from_after_symbol() {
        let mut s = Stoat::test();
        s.set_text("foo  ");
        s.set_cursor(0, 5); // After "foo  " (in trailing space)
        s.command("SelectPrevSymbol"); // Select "foo" with cursor on left
        s.assert_cursor_notation("<||foo|>  ");
    }

    #[test]
    fn select_between_symbols() {
        let mut s = Stoat::test();
        s.set_text("fn keyword");
        s.set_cursor(0, 7); // In "keyword"
        s.command("SelectPrevSymbol"); // Select from start of "keyword" to cursor, cursor on left
        s.assert_cursor_notation("fn <||keyw|>ord");
    }

    #[test]
    fn select_with_multiple_symbols() {
        let mut s = Stoat::test();
        s.set_text("let x = 42");
        s.set_cursor(0, 8); // After "x = " before "42"
        s.command("SelectPrevSymbol"); // Select "x" with cursor on left
        s.assert_cursor_notation("let <||x|> = 42");
    }

    #[test]
    fn skip_spaces_backward() {
        let mut s = Stoat::test();
        s.set_text("42   x");
        s.set_cursor(0, 6); // After "42   x"
        s.command("SelectPrevSymbol"); // Should skip spaces and select "42" with cursor on left
        s.assert_cursor_notation("<||42|>   x");
    }

    #[test]
    fn skip_newlines_backward() {
        let mut s = Stoat::test();
        s.set_text("foo\n\nx");
        s.set_cursor(1, 0); // Start of line 1 (after first newline)
        s.command("SelectPrevSymbol"); // Should select "foo" with cursor on left
        s.assert_cursor_notation("<||foo|>\n\nx");
    }

    #[test]
    fn at_start_of_buffer() {
        let mut s = Stoat::test();
        s.set_text("word");
        s.set_cursor(0, 0); // At start
        s.command("SelectPrevSymbol"); // No token to select
        s.assert_cursor_notation("|word"); // Cursor stays at start
    }

    #[test]
    fn mid_token_selects_from_start() {
        let mut s = Stoat::test();
        s.set_text("foo identifier");
        s.set_cursor(0, 8); // Middle of "identifier" (after "iden")
        s.command("SelectPrevSymbol"); // Select from start of "identifier" to cursor, cursor on left
        s.assert_cursor_notation("foo <||iden|>tifier");
    }
}
