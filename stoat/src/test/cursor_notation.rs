//! Test marker parsing for cursor and selection positions.
//!
//! This module provides a DSL for specifying cursor and selection positions
//! in test strings, making tests more readable and maintainable.
//!
//! # Syntax
//!
//! - `|` - Standalone cursor position
//! - `<||text|>` - Selection with cursor at start
//! - `<|text||>` - Selection with cursor at end
//!
//! # Escaping
//!
//! Outside selections, `||` escapes to a single `|`:
//! - `"||x|| + 1"` becomes `"|x| + 1"` (Rust closure)
//!
//! To include literal `<|` or `|>`, use triple pipes:
//! - `"<|||"` becomes `"<|"`
//! - `"|||>"` becomes `"|>"`
//!
//! # Examples
//!
//! ```ignore
//! use stoat::test::cursor_notation;
//!
//! // Cursor only
//! let p = cursor_notation::parse("hello |world").unwrap();
//! assert_eq!(p.text, "hello world");
//! assert_eq!(p.cursors, vec![6]);
//!
//! // Selection with cursor at end
//! let p = cursor_notation::parse("<|hello||>").unwrap();
//! assert_eq!(p.text, "hello");
//! assert_eq!(p.selections[0].range, 0..5);
//! assert!(!p.selections[0].cursor_at_start);
//! ```

use std::ops::Range;
use thiserror::Error;

/// Parsed marker information extracted from input string
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Text with all markers removed
    pub text: String,
    /// Byte offsets of standalone cursors
    pub cursors: Vec<usize>,
    /// Selections with cursor positions
    pub selections: Vec<Selection>,
}

/// A text selection with cursor at one end
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Byte range of the selection in the unmarked text
    pub range: Range<usize>,
    /// True if cursor is at start, false if at end
    pub cursor_at_start: bool,
}

/// Errors that can occur during marker parsing
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("Selection missing cursor marker (use <||text|> or <|text||>)")]
    SelectionMissingCursor,

    #[error("Cursor must be at selection boundary, not in middle")]
    CursorNotAtBoundary,

    #[error("Unclosed selection (missing |>)")]
    UnclosedSelection,

    #[error("Unexpected selection end |> without matching <|")]
    UnexpectedSelectionEnd,

    #[error("Selection has cursor at both start and end")]
    CursorAtBothEnds,
}

/// Parse a string with marker annotations
///
/// # Examples
///
/// ```ignore
/// let p = parse("|hello").unwrap();
/// assert_eq!(p.text, "hello");
/// assert_eq!(p.cursors, vec![0]);
/// ```
pub fn parse(input: &str) -> Result<Parsed, ParseError> {
    let mut text = String::new();
    let mut cursors = Vec::new();
    let mut selections = Vec::new();
    let mut chars = input.chars().peekable();
    let mut byte_offset = 0;

    // Track active selection
    let mut active_selection: Option<(usize, bool)> = None; // (start_offset, has_cursor_at_start)

    while let Some(ch) = chars.next() {
        match ch {
            '|' => {
                // Look ahead to determine token type
                match chars.peek() {
                    Some('|') => {
                        // Could be: ||, ||>
                        chars.next(); // consume second |

                        // Check what comes after
                        if chars.peek() == Some(&'>') {
                            // This is ||> - selection end with cursor
                            chars.next(); // consume >

                            if let Some((start, cursor_at_start)) = active_selection.take() {
                                if cursor_at_start {
                                    return Err(ParseError::CursorAtBothEnds);
                                }
                                selections.push(Selection {
                                    range: start..byte_offset,
                                    cursor_at_start: false,
                                });
                            } else {
                                return Err(ParseError::UnexpectedSelectionEnd);
                            }
                        } else {
                            // This is || - escaped pipe (outside selection)
                            if active_selection.is_some() {
                                return Err(ParseError::CursorNotAtBoundary);
                            }
                            text.push('|');
                            byte_offset += 1;
                        }
                    },
                    Some('>') => {
                        // This is |> - selection end without cursor
                        chars.next(); // consume >

                        if let Some((start, cursor_at_start)) = active_selection.take() {
                            if !cursor_at_start {
                                return Err(ParseError::SelectionMissingCursor);
                            }
                            selections.push(Selection {
                                range: start..byte_offset,
                                cursor_at_start: true,
                            });
                        } else {
                            return Err(ParseError::UnexpectedSelectionEnd);
                        }
                    },
                    _ => {
                        // Standalone | - cursor
                        if active_selection.is_some() {
                            return Err(ParseError::CursorNotAtBoundary);
                        }
                        cursors.push(byte_offset);
                    },
                }
            },
            '<' => {
                // Look ahead for <| or <||
                if chars.peek() == Some(&'|') {
                    // Peek further to distinguish <| from <||
                    let mut peek_iter = chars.clone();
                    peek_iter.next(); // skip first |
                    if peek_iter.peek() == Some(&'|') {
                        // This is <|| - selection start with cursor
                        chars.next(); // consume first |
                        chars.next(); // consume second |
                        if active_selection.is_some() {
                            return Err(ParseError::UnclosedSelection);
                        }
                        active_selection = Some((byte_offset, true));
                    } else {
                        // This is <| - selection start without cursor
                        chars.next(); // consume the |
                        if active_selection.is_some() {
                            return Err(ParseError::UnclosedSelection);
                        }
                        active_selection = Some((byte_offset, false));
                    }
                } else {
                    text.push('<');
                    byte_offset += ch.len_utf8();
                }
            },
            _ => {
                text.push(ch);
                byte_offset += ch.len_utf8();
            },
        }
    }

    if active_selection.is_some() {
        return Err(ParseError::UnclosedSelection);
    }

    Ok(Parsed {
        text,
        cursors,
        selections,
    })
}

/// Format text with markers for debugging/assertions
///
/// This is the inverse of [`parse`], converting cursor positions and selections
/// back into a marked string.
pub fn format(text: &str, cursors: &[usize], selections: &[Selection]) -> String {
    let mut markers = Vec::new();

    // Collect all markers with their positions and types
    for &cursor_offset in cursors {
        markers.push((cursor_offset, MarkerType::Cursor));
    }

    for sel in selections {
        if sel.cursor_at_start {
            markers.push((sel.range.start, MarkerType::SelectionStartWithCursor));
            markers.push((sel.range.end, MarkerType::SelectionEnd));
        } else {
            markers.push((sel.range.start, MarkerType::SelectionStart));
            markers.push((sel.range.end, MarkerType::SelectionEndWithCursor));
        }
    }

    // Sort by position (stable sort to preserve order of markers at same position)
    markers.sort_by_key(|(offset, _)| *offset);

    // Build output string
    let mut result = String::new();
    let mut last_offset = 0;

    for (offset, marker_type) in markers {
        // Add text before this marker
        result.push_str(&text[last_offset..offset]);

        // Add marker
        match marker_type {
            MarkerType::Cursor => result.push('|'),
            MarkerType::SelectionStart => result.push_str("<|"),
            MarkerType::SelectionStartWithCursor => result.push_str("<||"),
            MarkerType::SelectionEnd => result.push_str("|>"),
            MarkerType::SelectionEndWithCursor => result.push_str("||>"),
        }

        last_offset = offset;
    }

    // Add remaining text
    result.push_str(&text[last_offset..]);

    result
}

#[derive(Debug, Clone, Copy)]
enum MarkerType {
    Cursor,
    SelectionStart,
    SelectionStartWithCursor,
    SelectionEnd,
    SelectionEndWithCursor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cursor_only() {
        let p = parse("hello |world").unwrap();
        assert_eq!(p.text, "hello world");
        assert_eq!(p.cursors, vec![6]);
        assert!(p.selections.is_empty());
    }

    #[test]
    fn parse_cursor_at_start() {
        let p = parse("|hello").unwrap();
        assert_eq!(p.text, "hello");
        assert_eq!(p.cursors, vec![0]);
    }

    #[test]
    fn parse_cursor_at_end() {
        let p = parse("hello|").unwrap();
        assert_eq!(p.text, "hello");
        assert_eq!(p.cursors, vec![5]);
    }

    #[test]
    fn parse_selection_cursor_at_end() {
        let p = parse("<|hello||>").unwrap();
        assert_eq!(p.text, "hello");
        assert!(p.cursors.is_empty());
        assert_eq!(p.selections.len(), 1);
        assert_eq!(p.selections[0].range, 0..5);
        assert!(!p.selections[0].cursor_at_start);
    }

    #[test]
    fn parse_selection_cursor_at_start() {
        let p = parse("<||hello|>").unwrap();
        assert_eq!(p.text, "hello");
        assert!(p.cursors.is_empty());
        assert_eq!(p.selections.len(), 1);
        assert_eq!(p.selections[0].range, 0..5);
        assert!(p.selections[0].cursor_at_start);
    }

    #[test]
    fn parse_multi_cursor() {
        let p = parse("|foo |bar |baz").unwrap();
        assert_eq!(p.text, "foo bar baz");
        assert_eq!(p.cursors, vec![0, 4, 8]);
    }

    #[test]
    fn parse_cursor_and_selection() {
        let p = parse("|foo <|bar||>").unwrap();
        assert_eq!(p.text, "foo bar");
        assert_eq!(p.cursors, vec![0]);
        assert_eq!(p.selections.len(), 1);
        assert_eq!(p.selections[0].range, 4..7);
        assert!(!p.selections[0].cursor_at_start);
    }

    #[test]
    fn parse_escaped_pipe() {
        let p = parse("||x|| + 1").unwrap();
        assert_eq!(p.text, "|x| + 1");
        assert!(p.cursors.is_empty());
        assert!(p.selections.is_empty());
    }

    #[test]
    fn error_unclosed_selection() {
        let err = parse("<|hello").unwrap_err();
        assert_eq!(err, ParseError::UnclosedSelection);
    }

    #[test]
    fn error_unexpected_selection_end() {
        let err = parse("hello|>").unwrap_err();
        assert_eq!(err, ParseError::UnexpectedSelectionEnd);
    }

    #[test]
    fn error_cursor_in_middle() {
        let err = parse("<|hel|lo|>").unwrap_err();
        assert_eq!(err, ParseError::CursorNotAtBoundary);
    }

    #[test]
    fn error_selection_missing_cursor() {
        let err = parse("<|hello|>").unwrap_err();
        assert_eq!(err, ParseError::SelectionMissingCursor);
    }

    #[test]
    fn format_cursor_only() {
        let marked = format("hello world", &[6], &[]);
        assert_eq!(marked, "hello |world");
    }

    #[test]
    fn format_selection_cursor_at_end() {
        let sel = Selection {
            range: 0..5,
            cursor_at_start: false,
        };
        let marked = format("hello", &[], &[sel]);
        assert_eq!(marked, "<|hello||>");
    }

    #[test]
    fn format_selection_cursor_at_start() {
        let sel = Selection {
            range: 0..5,
            cursor_at_start: true,
        };
        let marked = format("hello", &[], &[sel]);
        assert_eq!(marked, "<||hello|>");
    }

    #[test]
    fn round_trip_cursor() {
        let input = "hello |world";
        let parsed = parse(input).unwrap();
        let formatted = format(&parsed.text, &parsed.cursors, &parsed.selections);
        assert_eq!(formatted, input);
    }

    #[test]
    fn round_trip_selection() {
        let input = "<|hello||>";
        let parsed = parse(input).unwrap();
        let formatted = format(&parsed.text, &parsed.cursors, &parsed.selections);
        assert_eq!(formatted, input);
    }
}
