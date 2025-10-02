//! Select next symbol command
//!
//! Finds and selects the next symbol (identifier, keyword, or literal) from the cursor position,
//! automatically skipping whitespace, punctuation, and operators. This enables semantic navigation
//! through code by jumping between meaningful named entities.

use crate::Stoat;
use gpui::App;
use std::ops::Range;

impl Stoat {
    /// Select the next symbol from the current cursor position.
    ///
    /// Skips whitespace, punctuation, and operators to find the next alphanumeric token
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
    /// - If cursor is mid-symbol, selects remainder of current symbol
    /// - If no next symbol exists, returns None
    ///
    /// # Returns
    ///
    /// The byte range of the selected symbol, or None if no symbol found.
    ///
    /// # Related
    ///
    /// See also [`crate::selection::select_next_token`] for token-level selection that
    /// includes punctuation and operators.
    pub fn select_next_symbol(&mut self, cx: &App) -> Option<Range<usize>> {
        use text::ToOffset;

        let buffer_snapshot = self.buffer_snapshot(cx);
        let token_snapshot = self.token_snapshot();
        let cursor_pos = self.cursor_manager.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol = None;

        // Track the first symbol we encounter at cursor position (fallback)
        let mut symbol_at_cursor = None;

        // Iterate through tokens to find the next symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If we're at the start of a symbol, remember it as fallback
                // but try to find the next symbol first
                if token_start == cursor_offset && symbol_at_cursor.is_none() {
                    symbol_at_cursor = Some((token_start, token_end));
                    token_cursor.next();
                    continue;
                }

                // Found a symbol after the cursor position
                let selection_start = cursor_offset.max(token_start);
                found_symbol = Some(selection_start..token_end);
                break;
            }

            // Not a symbol, keep looking
            token_cursor.next();
        }

        // If we didn't find a symbol after the cursor, use the one at cursor (if any)
        if found_symbol.is_none() {
            if let Some((start, end)) = symbol_at_cursor {
                found_symbol = Some(start..end);
            }
        }

        // If we found a symbol, update the cursor and selection
        if let Some(ref range) = found_symbol {
            let selection_start = buffer_snapshot.offset_to_point(range.start);
            let selection_end = buffer_snapshot.offset_to_point(range.end);

            // Create the selection
            let selection = crate::cursor::Selection::new(selection_start, selection_end);
            self.cursor_manager.set_selection(selection);
        }

        found_symbol
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn select_symbol() {
        let mut s = Stoat::test();
        s.set_text("fn foo() -> Result<()>");
        s.input("w"); // Select "foo"
        s.assert_cursor_notation("fn <|foo||>() -> Result<()>");
        s.input("w"); // Select "Result"
        s.assert_cursor_notation("fn foo() -> <|Result||><()>");
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

    #[test]
    fn select_number_token() {
        let mut s = Stoat::test();
        s.set_text("let x = 42");
        s.set_cursor(0, 8); // Position at start of "42"
        s.input("w"); // Select next token
        s.assert_cursor_notation("let x = <|42||>");
    }

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
