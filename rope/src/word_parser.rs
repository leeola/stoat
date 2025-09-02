//! Word boundary parsing for identifiers and text
//!
//! This module provides utilities for parsing word boundaries within identifiers
//! and text tokens, supporting various naming conventions like camelCase, snake_case, etc.

use std::ops::Range;

/// Pattern type detected in an identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordPattern {
    /// camelCase: fooBarBaz
    CamelCase,
    /// PascalCase: FooBarBaz
    PascalCase,
    /// snake_case: foo_bar_baz
    SnakeCase,
    /// SCREAMING_SNAKE_CASE: FOO_BAR_BAZ
    ScreamingSnake,
    /// kebab-case: foo-bar-baz
    KebabCase,
    /// Single word: foo
    SingleWord,
    /// Mixed or unknown pattern
    Mixed,
}

/// Part of an identifier - either a word or separator
#[derive(Debug, Clone, PartialEq)]
pub enum IdentifierPart {
    Word(String, Range<usize>),
    Separator(String, Range<usize>),
}

/// Parse word boundaries in an identifier or text
///
/// Returns a vector of parts (words and separators) found within the input text.
/// This preserves separators like underscores and dashes for round-trip accuracy.
///
/// # Examples
///
/// ```
/// use stoat_rope::word_parser::parse_identifier_parts;
///
/// let parts = parse_identifier_parts("foo_bar");
/// assert_eq!(parts.len(), 3); // "foo", "_", "bar"
/// ```
pub fn parse_identifier_parts(text: &str) -> Vec<IdentifierPart> {
    if text.is_empty() {
        return vec![];
    }

    let mut parts = Vec::new();
    let mut current_word = String::new();
    let mut word_start = 0;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        let next_char = chars.peek().map(|(_, c)| *c);

        // Check for separator characters
        if ch == '_' || ch == '-' {
            // End current word if not empty
            if !current_word.is_empty() {
                parts.push(IdentifierPart::Word(current_word.clone(), word_start..idx));
                current_word.clear();
            }
            // Add separator as its own part
            parts.push(IdentifierPart::Separator(
                ch.to_string(),
                idx..idx + ch.len_utf8(),
            ));
            word_start = idx + ch.len_utf8();
            continue;
        }

        // Handle camelCase and PascalCase transitions
        if let Some(next) = next_char {
            let current_is_lower = ch.is_ascii_lowercase();
            let current_is_upper = ch.is_ascii_uppercase();
            let next_is_lower = next.is_ascii_lowercase();
            let next_is_upper = next.is_ascii_uppercase();

            // Transition from lowercase to uppercase (camelCase boundary)
            if current_is_lower && next_is_upper {
                current_word.push(ch);
                parts.push(IdentifierPart::Word(
                    current_word.clone(),
                    word_start..idx + ch.len_utf8(),
                ));
                current_word.clear();
                word_start = idx + ch.len_utf8();
                continue;
            }

            // Transition from multiple uppercase to lowercase (XMLHttp -> XML, Http)
            if current_is_upper && next_is_lower && !current_word.is_empty() {
                // Don't split if this is the first character of a word
                if word_start != idx {
                    parts.push(IdentifierPart::Word(current_word.clone(), word_start..idx));
                    current_word.clear();
                    current_word.push(ch);
                    word_start = idx;
                    continue;
                }
            }
        }

        // Add character to current word
        current_word.push(ch);
    }

    // Add final word if not empty
    if !current_word.is_empty() {
        parts.push(IdentifierPart::Word(current_word, word_start..text.len()));
    }

    parts
}

/// Parse word boundaries in an identifier or text (legacy interface)
///
/// Returns a vector of (word_text, byte_range) tuples representing
/// the individual words found within the input text.
/// Note: This does NOT preserve separators - use parse_identifier_parts for that.
pub fn parse_identifier_words(text: &str) -> Vec<(String, Range<usize>)> {
    if text.is_empty() {
        return vec![];
    }

    let mut words = Vec::new();
    let mut current_word = String::new();
    let mut word_start = 0;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        let next_char = chars.peek().map(|(_, c)| *c);

        // Check for separator characters
        if ch == '_' || ch == '-' {
            // End current word if not empty
            if !current_word.is_empty() {
                words.push((current_word.clone(), word_start..idx));
                current_word.clear();
            }
            // Skip separator, next word starts after it
            word_start = idx + ch.len_utf8();
            continue;
        }

        // Handle camelCase and PascalCase transitions
        if let Some(next) = next_char {
            let current_is_lower = ch.is_ascii_lowercase();
            let current_is_upper = ch.is_ascii_uppercase();
            let next_is_lower = next.is_ascii_lowercase();
            let next_is_upper = next.is_ascii_uppercase();

            // Transition from lowercase to uppercase (camelCase boundary)
            if current_is_lower && next_is_upper {
                current_word.push(ch);
                words.push((current_word.clone(), word_start..idx + ch.len_utf8()));
                current_word.clear();
                word_start = idx + ch.len_utf8();
                continue;
            }

            // Transition from multiple uppercase to lowercase (XMLHttp -> XML, Http)
            if current_is_upper && next_is_lower && !current_word.is_empty() {
                // Don't split if this is the first character of a word
                if word_start != idx {
                    words.push((current_word.clone(), word_start..idx));
                    current_word.clear();
                    current_word.push(ch);
                    word_start = idx;
                    continue;
                }
            }
        }

        // Add character to current word
        current_word.push(ch);
    }

    // Add final word if not empty
    if !current_word.is_empty() {
        words.push((current_word, word_start..text.len()));
    }

    words
}

/// Detect the pattern type of an identifier
pub fn detect_word_pattern(text: &str) -> WordPattern {
    if text.is_empty() {
        return WordPattern::SingleWord;
    }

    let has_underscore = text.contains('_');
    let has_dash = text.contains('-');
    let has_uppercase = text.chars().any(|c| c.is_ascii_uppercase());
    let has_lowercase = text.chars().any(|c| c.is_ascii_lowercase());
    let starts_with_uppercase = text
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_uppercase());

    match (has_underscore, has_dash, has_uppercase, has_lowercase) {
        // snake_case or SCREAMING_SNAKE_CASE
        (true, false, false, true) => WordPattern::SnakeCase,
        (true, false, true, false) => WordPattern::ScreamingSnake,

        // kebab-case
        (false, true, false, true) => WordPattern::KebabCase,

        // camelCase or PascalCase
        (false, false, true, true) => {
            if starts_with_uppercase {
                WordPattern::PascalCase
            } else {
                WordPattern::CamelCase
            }
        },

        // Single word (all lowercase or all uppercase)
        (false, false, false, true) | (false, false, true, false) => WordPattern::SingleWord,

        // Mixed or unknown
        _ => WordPattern::Mixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_camel_case() {
        let words = parse_identifier_words("fooBarBaz");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("foo".to_string(), 0..3));
        assert_eq!(words[1], ("Bar".to_string(), 3..6));
        assert_eq!(words[2], ("Baz".to_string(), 6..9));
    }

    #[test]
    fn parse_pascal_case() {
        let words = parse_identifier_words("FooBarBaz");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("Foo".to_string(), 0..3));
        assert_eq!(words[1], ("Bar".to_string(), 3..6));
        assert_eq!(words[2], ("Baz".to_string(), 6..9));
    }

    #[test]
    fn parse_snake_case() {
        let words = parse_identifier_words("foo_bar_baz");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("foo".to_string(), 0..3));
        assert_eq!(words[1], ("bar".to_string(), 4..7));
        assert_eq!(words[2], ("baz".to_string(), 8..11));
    }

    #[test]
    fn parse_screaming_snake() {
        let words = parse_identifier_words("FOO_BAR_BAZ");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("FOO".to_string(), 0..3));
        assert_eq!(words[1], ("BAR".to_string(), 4..7));
        assert_eq!(words[2], ("BAZ".to_string(), 8..11));
    }

    #[test]
    fn parse_kebab_case() {
        let words = parse_identifier_words("foo-bar-baz");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("foo".to_string(), 0..3));
        assert_eq!(words[1], ("bar".to_string(), 4..7));
        assert_eq!(words[2], ("baz".to_string(), 8..11));
    }

    #[test]
    fn parse_mixed_case() {
        let words = parse_identifier_words("XMLHttpRequest");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], ("XML".to_string(), 0..3));
        assert_eq!(words[1], ("Http".to_string(), 3..7));
        assert_eq!(words[2], ("Request".to_string(), 7..14));
    }

    #[test]
    fn parse_single_word() {
        let words = parse_identifier_words("foo");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0], ("foo".to_string(), 0..3));

        let words = parse_identifier_words("FOO");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0], ("FOO".to_string(), 0..3));
    }

    #[test]
    fn parse_empty() {
        let words = parse_identifier_words("");
        assert_eq!(words.len(), 0);
    }

    #[test]
    fn detect_patterns() {
        assert_eq!(detect_word_pattern("fooBarBaz"), WordPattern::CamelCase);
        assert_eq!(detect_word_pattern("FooBarBaz"), WordPattern::PascalCase);
        assert_eq!(detect_word_pattern("foo_bar_baz"), WordPattern::SnakeCase);
        assert_eq!(
            detect_word_pattern("FOO_BAR_BAZ"),
            WordPattern::ScreamingSnake
        );
        assert_eq!(detect_word_pattern("foo-bar-baz"), WordPattern::KebabCase);
        assert_eq!(detect_word_pattern("foo"), WordPattern::SingleWord);
        assert_eq!(detect_word_pattern("FOO"), WordPattern::SingleWord);
        assert_eq!(detect_word_pattern("foo_Bar-baz"), WordPattern::Mixed);
    }
}
