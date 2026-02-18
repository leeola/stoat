use std::ops::Range;
use text::BufferSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharKind {
    Word,
    Punctuation,
    Whitespace,
}

/// Classifies characters for word-motion boundary detection.
///
/// Stateless for now; future: language-specific word chars (e.g. `-` in CSS).
pub struct CharClassifier;

impl CharClassifier {
    pub fn kind(ch: char) -> CharKind {
        if ch.is_alphanumeric() || ch == '_' {
            CharKind::Word
        } else if ch.is_whitespace() {
            CharKind::Whitespace
        } else {
            CharKind::Punctuation
        }
    }

    /// Move forward to end of current/next word group.
    ///
    /// If on a Word or Punctuation char, skips to end of that group.
    /// If on Whitespace, skips whitespace then skips the next non-whitespace group.
    pub fn next_word_end(snapshot: &BufferSnapshot, offset: usize) -> usize {
        let len = snapshot.len();
        if offset >= len {
            return offset;
        }

        let mut pos = offset;
        let mut chars = snapshot.chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        let kind = Self::kind(first);
        pos += first.len_utf8();

        match kind {
            CharKind::Whitespace => {
                // Skip remaining whitespace
                loop {
                    let Some(ch) = chars.next() else {
                        return pos;
                    };
                    if Self::kind(ch) != CharKind::Whitespace {
                        let group_kind = Self::kind(ch);
                        pos += ch.len_utf8();
                        for ch in chars {
                            if Self::kind(ch) != group_kind {
                                break;
                            }
                            pos += ch.len_utf8();
                        }
                        return pos;
                    }
                    pos += ch.len_utf8();
                }
            },
            _ => {
                for ch in chars {
                    if Self::kind(ch) != kind {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                pos
            },
        }
    }

    /// Move forward to end of current/next WORD group (whitespace-delimited).
    ///
    /// Treats all non-whitespace characters as the same class. Only whitespace
    /// separates WORD boundaries (like vim's `E` motion).
    pub fn next_word_end_big(snapshot: &BufferSnapshot, offset: usize) -> usize {
        let len = snapshot.len();
        if offset >= len {
            return offset;
        }

        let mut pos = offset;
        let mut chars = snapshot.chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        let is_ws = first.is_whitespace();
        pos += first.len_utf8();

        if is_ws {
            // Skip remaining whitespace, then skip non-whitespace
            loop {
                let Some(ch) = chars.next() else {
                    return pos;
                };
                if !ch.is_whitespace() {
                    pos += ch.len_utf8();
                    for ch in chars {
                        if ch.is_whitespace() {
                            break;
                        }
                        pos += ch.len_utf8();
                    }
                    return pos;
                }
                pos += ch.len_utf8();
            }
        } else {
            // Skip remaining non-whitespace
            for ch in chars {
                if ch.is_whitespace() {
                    break;
                }
                pos += ch.len_utf8();
            }
            pos
        }
    }

    /// Move backward to start of current/previous word group.
    ///
    /// If preceding char is Word or Punctuation, skips backward through that group.
    /// If preceding char is Whitespace, skips whitespace then skips the previous group.
    pub fn previous_word_start(snapshot: &BufferSnapshot, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }

        let mut pos = offset;
        let mut chars = snapshot.reversed_chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        let kind = Self::kind(first);
        pos -= first.len_utf8();

        match kind {
            CharKind::Whitespace => loop {
                let Some(ch) = chars.next() else {
                    return pos;
                };
                if Self::kind(ch) != CharKind::Whitespace {
                    let group_kind = Self::kind(ch);
                    pos -= ch.len_utf8();
                    for ch in chars {
                        if Self::kind(ch) != group_kind {
                            break;
                        }
                        pos -= ch.len_utf8();
                    }
                    return pos;
                }
                pos -= ch.len_utf8();
            },
            _ => {
                for ch in chars {
                    if Self::kind(ch) != kind {
                        break;
                    }
                    pos -= ch.len_utf8();
                }
                pos
            },
        }
    }

    /// Move forward to start of next word group.
    ///
    /// From offset, skips the rest of the current group (if any), then skips whitespace,
    /// returning where the next group starts. This is the `w` motion target.
    pub fn next_word_start(snapshot: &BufferSnapshot, offset: usize) -> usize {
        let len = snapshot.len();
        if offset >= len {
            return offset;
        }

        let mut pos = offset;
        let mut chars = snapshot.chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        let kind = Self::kind(first);
        pos += first.len_utf8();

        // Skip rest of current group
        if kind != CharKind::Whitespace {
            for ch in chars.by_ref() {
                if Self::kind(ch) != kind {
                    if Self::kind(ch) == CharKind::Whitespace {
                        pos += ch.len_utf8();
                        break;
                    }
                    return pos;
                }
                pos += ch.len_utf8();
            }
        }

        // Skip whitespace
        for ch in chars {
            if !ch.is_whitespace() {
                return pos;
            }
            pos += ch.len_utf8();
        }

        pos
    }

    /// Move forward to start of next WORD (whitespace-delimited).
    ///
    /// Treats all non-whitespace characters as the same class. Skips the rest of the
    /// current WORD then skips whitespace. This is the `W` motion target.
    pub fn next_word_start_big(snapshot: &BufferSnapshot, offset: usize) -> usize {
        let len = snapshot.len();
        if offset >= len {
            return offset;
        }

        let mut pos = offset;
        let mut chars = snapshot.chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        pos += first.len_utf8();

        if !first.is_whitespace() {
            // Skip rest of current non-whitespace group
            for ch in chars.by_ref() {
                if ch.is_whitespace() {
                    pos += ch.len_utf8();
                    break;
                }
                pos += ch.len_utf8();
            }
        }

        // Skip whitespace
        for ch in chars {
            if !ch.is_whitespace() {
                return pos;
            }
            pos += ch.len_utf8();
        }

        pos
    }

    /// Move backward to start of current/previous WORD (whitespace-delimited).
    ///
    /// Treats all non-whitespace characters as the same class. Only whitespace
    /// separates WORD boundaries. This is the `B` motion target.
    pub fn previous_word_start_big(snapshot: &BufferSnapshot, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }

        let mut pos = offset;
        let mut chars = snapshot.reversed_chars_at(offset);

        let Some(first) = chars.next() else {
            return offset;
        };
        let is_ws = first.is_whitespace();
        pos -= first.len_utf8();

        if is_ws {
            // Skip remaining whitespace, then skip non-whitespace
            loop {
                let Some(ch) = chars.next() else {
                    return pos;
                };
                if !ch.is_whitespace() {
                    pos -= ch.len_utf8();
                    for ch in chars {
                        if ch.is_whitespace() {
                            break;
                        }
                        pos -= ch.len_utf8();
                    }
                    return pos;
                }
                pos -= ch.len_utf8();
            }
        } else {
            for ch in chars {
                if ch.is_whitespace() {
                    break;
                }
                pos -= ch.len_utf8();
            }
            pos
        }
    }

    /// Find the range of the next Word-class group at or after offset.
    ///
    /// If the cursor is at the start of a word, returns that word's range.
    /// If inside a word, skips to the next word. Skips punctuation and whitespace.
    pub fn next_word_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
        let len = snapshot.len();
        if offset >= len {
            return None;
        }

        let first_kind = snapshot.chars_at(offset).next().map(Self::kind)?;
        let prev_kind = if offset > 0 {
            snapshot.reversed_chars_at(offset).next().map(Self::kind)
        } else {
            None
        };

        let at_word_start =
            first_kind == CharKind::Word && prev_kind.is_none_or(|k| k != CharKind::Word);

        if at_word_start {
            let mut end = offset;
            for ch in snapshot.chars_at(offset) {
                if Self::kind(ch) != CharKind::Word {
                    break;
                }
                end += ch.len_utf8();
            }
            return Some(offset..end);
        }

        // Skip past current word (if inside one) then find next word
        let search_start = if first_kind == CharKind::Word {
            let mut pos = offset;
            for ch in snapshot.chars_at(offset) {
                if Self::kind(ch) != CharKind::Word {
                    break;
                }
                pos += ch.len_utf8();
            }
            pos
        } else {
            offset
        };

        let mut pos = search_start;
        let mut chars = snapshot.chars_at(search_start);
        loop {
            let ch = chars.next()?;
            if Self::kind(ch) == CharKind::Word {
                let start = pos;
                pos += ch.len_utf8();
                for ch in chars {
                    if Self::kind(ch) != CharKind::Word {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                return Some(start..pos);
            }
            pos += ch.len_utf8();
        }
    }

    /// Find the range of the previous Word-class group before offset.
    ///
    /// Returns the full word range even if the cursor is in the middle of the word.
    pub fn prev_word_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
        if offset == 0 {
            return None;
        }

        let mut pos = offset;
        let mut reversed = snapshot.reversed_chars_at(offset);

        // Skip non-Word chars backward to find a Word char
        loop {
            let ch = reversed.next()?;
            if Self::kind(ch) == CharKind::Word {
                pos -= ch.len_utf8();
                break;
            }
            pos -= ch.len_utf8();
        }

        // Continue backward to find start of this word
        for ch in reversed {
            if Self::kind(ch) != CharKind::Word {
                break;
            }
            pos -= ch.len_utf8();
        }
        let start = pos;

        // Go forward to find end of this word
        let mut end = start;
        for ch in snapshot.chars_at(start) {
            if Self::kind(ch) != CharKind::Word {
                break;
            }
            end += ch.len_utf8();
        }

        Some(start..end)
    }

    /// Find the range of the next non-whitespace group at or after offset.
    ///
    /// Groups are contiguous runs of the same [`CharKind`] (Word or Punctuation).
    /// If inside a group, skips to the next one.
    pub fn next_group_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
        let len = snapshot.len();
        if offset >= len {
            return None;
        }

        let first_kind = snapshot.chars_at(offset).next().map(Self::kind)?;
        let prev_kind = if offset > 0 {
            snapshot.reversed_chars_at(offset).next().map(Self::kind)
        } else {
            None
        };

        let at_group_start =
            first_kind != CharKind::Whitespace && prev_kind.is_none_or(|k| k != first_kind);

        let search_start = if at_group_start {
            offset
        } else if first_kind != CharKind::Whitespace {
            // In middle of a group, skip to its end
            let mut pos = offset;
            for ch in snapshot.chars_at(offset) {
                if Self::kind(ch) != first_kind {
                    break;
                }
                pos += ch.len_utf8();
            }
            pos
        } else {
            offset
        };

        let mut pos = search_start;
        let mut chars = snapshot.chars_at(search_start);
        loop {
            let ch = chars.next()?;
            if Self::kind(ch) != CharKind::Whitespace {
                let start = pos;
                let group_kind = Self::kind(ch);
                pos += ch.len_utf8();
                for ch in chars {
                    if Self::kind(ch) != group_kind {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                return Some(start..pos);
            }
            pos += ch.len_utf8();
        }
    }

    /// Find the range of the previous non-whitespace group before offset.
    ///
    /// Returns the full group range even if the cursor is in the middle.
    pub fn prev_group_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
        if offset == 0 {
            return None;
        }

        let mut pos = offset;
        let mut reversed = snapshot.reversed_chars_at(offset);

        // Skip whitespace backward, find a non-whitespace char
        let group_kind;
        loop {
            let ch = reversed.next()?;
            if Self::kind(ch) != CharKind::Whitespace {
                group_kind = Self::kind(ch);
                pos -= ch.len_utf8();
                break;
            }
            pos -= ch.len_utf8();
        }

        // Continue backward to find start of this group
        for ch in reversed {
            if Self::kind(ch) != group_kind {
                break;
            }
            pos -= ch.len_utf8();
        }
        let start = pos;

        // Go forward to find end
        let mut end = start;
        for ch in snapshot.chars_at(start) {
            if Self::kind(ch) != group_kind {
                break;
            }
            end += ch.len_utf8();
        }

        Some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Buffer;

    fn snapshot(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, text::BufferId::new(1).unwrap(), text);
        buffer.snapshot()
    }

    #[test]
    fn kind_classification() {
        assert_eq!(CharClassifier::kind('a'), CharKind::Word);
        assert_eq!(CharClassifier::kind('Z'), CharKind::Word);
        assert_eq!(CharClassifier::kind('0'), CharKind::Word);
        assert_eq!(CharClassifier::kind('_'), CharKind::Word);
        assert_eq!(CharClassifier::kind(' '), CharKind::Whitespace);
        assert_eq!(CharClassifier::kind('\n'), CharKind::Whitespace);
        assert_eq!(CharClassifier::kind('\t'), CharKind::Whitespace);
        assert_eq!(CharClassifier::kind('.'), CharKind::Punctuation);
        assert_eq!(CharClassifier::kind(','), CharKind::Punctuation);
        assert_eq!(CharClassifier::kind('('), CharKind::Punctuation);
        assert_eq!(CharClassifier::kind('+'), CharKind::Punctuation);
    }

    #[test]
    fn next_word_end_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::next_word_end(&s, 0), 5);
        assert_eq!(CharClassifier::next_word_end(&s, 3), 5);
        assert_eq!(CharClassifier::next_word_end(&s, 5), 11);
        assert_eq!(CharClassifier::next_word_end(&s, 6), 11);
        assert_eq!(CharClassifier::next_word_end(&s, 11), 11);
    }

    #[test]
    fn next_word_end_punctuation() {
        let s = snapshot("hello, world");
        assert_eq!(CharClassifier::next_word_end(&s, 0), 5);
        assert_eq!(CharClassifier::next_word_end(&s, 5), 6);
        assert_eq!(CharClassifier::next_word_end(&s, 6), 12);
    }

    #[test]
    fn next_word_end_empty() {
        let s = snapshot("");
        assert_eq!(CharClassifier::next_word_end(&s, 0), 0);
    }

    #[test]
    fn next_word_end_all_whitespace() {
        let s = snapshot("   ");
        assert_eq!(CharClassifier::next_word_end(&s, 0), 3);
    }

    #[test]
    fn previous_word_start_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::previous_word_start(&s, 11), 6);
        assert_eq!(CharClassifier::previous_word_start(&s, 8), 6);
        assert_eq!(CharClassifier::previous_word_start(&s, 6), 0);
        assert_eq!(CharClassifier::previous_word_start(&s, 5), 0);
        assert_eq!(CharClassifier::previous_word_start(&s, 0), 0);
    }

    #[test]
    fn previous_word_start_punctuation() {
        let s = snapshot("hello, world");
        assert_eq!(CharClassifier::previous_word_start(&s, 12), 7);
        assert_eq!(CharClassifier::previous_word_start(&s, 7), 5);
        assert_eq!(CharClassifier::previous_word_start(&s, 6), 5);
        assert_eq!(CharClassifier::previous_word_start(&s, 5), 0);
    }

    #[test]
    fn next_word_range_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::next_word_range(&s, 0), Some(0..5));
        assert_eq!(CharClassifier::next_word_range(&s, 3), Some(6..11));
        assert_eq!(CharClassifier::next_word_range(&s, 5), Some(6..11));
        assert_eq!(CharClassifier::next_word_range(&s, 6), Some(6..11));
        assert_eq!(CharClassifier::next_word_range(&s, 11), None);
    }

    #[test]
    fn next_word_range_punctuation() {
        let s = snapshot("foo.bar");
        assert_eq!(CharClassifier::next_word_range(&s, 0), Some(0..3));
        assert_eq!(CharClassifier::next_word_range(&s, 3), Some(4..7));
        assert_eq!(CharClassifier::next_word_range(&s, 4), Some(4..7));
    }

    #[test]
    fn prev_word_range_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::prev_word_range(&s, 11), Some(6..11));
        assert_eq!(CharClassifier::prev_word_range(&s, 8), Some(6..11));
        assert_eq!(CharClassifier::prev_word_range(&s, 6), Some(0..5));
        assert_eq!(CharClassifier::prev_word_range(&s, 5), Some(0..5));
        assert_eq!(CharClassifier::prev_word_range(&s, 0), None);
    }

    #[test]
    fn prev_word_range_punctuation() {
        let s = snapshot("foo.bar");
        assert_eq!(CharClassifier::prev_word_range(&s, 7), Some(4..7));
        assert_eq!(CharClassifier::prev_word_range(&s, 4), Some(0..3));
        assert_eq!(CharClassifier::prev_word_range(&s, 3), Some(0..3));
    }

    #[test]
    fn next_group_range_basic() {
        let s = snapshot("foo.bar baz");
        assert_eq!(CharClassifier::next_group_range(&s, 0), Some(0..3));
        assert_eq!(CharClassifier::next_group_range(&s, 3), Some(3..4));
        assert_eq!(CharClassifier::next_group_range(&s, 4), Some(4..7));
        assert_eq!(CharClassifier::next_group_range(&s, 7), Some(8..11));
    }

    #[test]
    fn next_group_range_in_middle() {
        let s = snapshot("foo.bar");
        assert_eq!(CharClassifier::next_group_range(&s, 1), Some(3..4));
    }

    #[test]
    fn prev_group_range_basic() {
        let s = snapshot("foo.bar baz");
        assert_eq!(CharClassifier::prev_group_range(&s, 11), Some(8..11));
        assert_eq!(CharClassifier::prev_group_range(&s, 8), Some(4..7));
        assert_eq!(CharClassifier::prev_group_range(&s, 7), Some(4..7));
        assert_eq!(CharClassifier::prev_group_range(&s, 4), Some(3..4));
        assert_eq!(CharClassifier::prev_group_range(&s, 3), Some(0..3));
    }

    #[test]
    fn multiline() {
        let s = snapshot("hello\nworld");
        assert_eq!(CharClassifier::next_word_end(&s, 0), 5);
        assert_eq!(CharClassifier::next_word_end(&s, 5), 11);
        assert_eq!(CharClassifier::previous_word_start(&s, 11), 6);
        assert_eq!(CharClassifier::previous_word_start(&s, 6), 0);
    }

    #[test]
    fn next_word_start_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::next_word_start(&s, 0), 6);
        assert_eq!(CharClassifier::next_word_start(&s, 3), 6);
        assert_eq!(CharClassifier::next_word_start(&s, 5), 6);
        assert_eq!(CharClassifier::next_word_start(&s, 6), 11);
        assert_eq!(CharClassifier::next_word_start(&s, 11), 11);
    }

    #[test]
    fn next_word_start_punctuation() {
        let s = snapshot("hello.world");
        assert_eq!(CharClassifier::next_word_start(&s, 0), 5);
        assert_eq!(CharClassifier::next_word_start(&s, 5), 6);
        assert_eq!(CharClassifier::next_word_start(&s, 6), 11);
    }

    #[test]
    fn next_word_start_mixed() {
        let s = snapshot("foo, bar");
        assert_eq!(CharClassifier::next_word_start(&s, 0), 3);
        assert_eq!(CharClassifier::next_word_start(&s, 3), 5);
    }

    #[test]
    fn next_word_start_big_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::next_word_start_big(&s, 0), 6);
        assert_eq!(CharClassifier::next_word_start_big(&s, 6), 11);
    }

    #[test]
    fn next_word_start_big_punctuation() {
        let s = snapshot("hello.world foo");
        assert_eq!(CharClassifier::next_word_start_big(&s, 0), 12);
        assert_eq!(CharClassifier::next_word_start_big(&s, 5), 12);
    }

    #[test]
    fn previous_word_start_big_basic() {
        let s = snapshot("hello world");
        assert_eq!(CharClassifier::previous_word_start_big(&s, 11), 6);
        assert_eq!(CharClassifier::previous_word_start_big(&s, 6), 0);
        assert_eq!(CharClassifier::previous_word_start_big(&s, 5), 0);
        assert_eq!(CharClassifier::previous_word_start_big(&s, 0), 0);
    }

    #[test]
    fn previous_word_start_big_punctuation() {
        let s = snapshot("hello.world foo");
        assert_eq!(CharClassifier::previous_word_start_big(&s, 15), 12);
        assert_eq!(CharClassifier::previous_word_start_big(&s, 12), 0);
        assert_eq!(CharClassifier::previous_word_start_big(&s, 11), 0);
    }
}
