use crate::Rope;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CharCategory {
    Whitespace,
    Eol,
    Word,
    Punctuation,
    Unknown,
}

pub fn categorize_char(ch: char) -> CharCategory {
    if char_is_line_ending(ch) {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else if char_is_word(ch) {
        CharCategory::Word
    } else if ch.is_ascii_punctuation() {
        CharCategory::Punctuation
    } else {
        CharCategory::Unknown
    }
}

fn char_is_line_ending(ch: char) -> bool {
    matches!(ch, '\n' | '\r')
}

fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn long_word_category(ch: char) -> CharCategory {
    if char_is_line_ending(ch) {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else {
        CharCategory::Word
    }
}

pub fn next_word_start(rope: &Rope, from: usize) -> usize {
    next_word_start_with(rope, from, categorize_char)
}

pub fn next_long_word_start(rope: &Rope, from: usize) -> usize {
    next_word_start_with(rope, from, long_word_category)
}

fn next_word_start_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    from: usize,
    category: F,
) -> usize {
    let mut chars = rope.chars_at(from);
    let Some(first_char) = chars.next() else {
        return from;
    };
    let head_start = from + first_char.len_utf8();
    let mut head = head_start;
    let mut prev_ch = first_char;

    loop {
        let Some(ch) = chars.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (char_is_line_ending(ch) || !ch.is_whitespace());
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head += ch.len_utf8();
    }
}

pub fn next_word_end(rope: &Rope, from: usize) -> usize {
    next_word_end_with(rope, from, categorize_char)
}

pub fn next_long_word_end(rope: &Rope, from: usize) -> usize {
    next_word_end_with(rope, from, long_word_category)
}

fn next_word_end_with<F: Fn(char) -> CharCategory>(rope: &Rope, from: usize, category: F) -> usize {
    let mut chars = rope.chars_at(from);
    let Some(first_char) = chars.next() else {
        return from;
    };
    let head_start = from + first_char.len_utf8();
    let mut head = head_start;
    let mut prev_ch = first_char;

    loop {
        let Some(ch) = chars.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head += ch.len_utf8();
    }
}

pub fn prev_word_start(rope: &Rope, from: usize) -> usize {
    prev_word_start_with(rope, from, categorize_char)
}

pub fn prev_long_word_start(rope: &Rope, from: usize) -> usize {
    prev_word_start_with(rope, from, long_word_category)
}

fn prev_word_start_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    from: usize,
    category: F,
) -> usize {
    if from == 0 {
        return 0;
    }
    let Some(first_char) = rope.chars_at(from).next() else {
        return prev_word_start_from_end(rope, from, &category);
    };
    let head_start = from;
    let mut head = head_start;
    let mut prev_ch = first_char;
    let mut iter = rope.reversed_chars_at(from);

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

fn prev_word_start_from_end<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    from: usize,
    category: &F,
) -> usize {
    let head_start = from;
    let mut head = head_start;
    let mut iter = rope.reversed_chars_at(from);
    let Some(seed) = iter.next() else {
        return head;
    };
    head -= seed.len_utf8();
    let mut prev_ch = seed;

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start - seed.len_utf8() {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

pub fn prev_word_end(rope: &Rope, from: usize) -> usize {
    prev_word_end_with(rope, from, categorize_char)
}

pub fn prev_long_word_end(rope: &Rope, from: usize) -> usize {
    prev_word_end_with(rope, from, long_word_category)
}

fn prev_word_end_with<F: Fn(char) -> CharCategory>(rope: &Rope, from: usize, category: F) -> usize {
    if from == 0 {
        return 0;
    }
    let Some(first_char) = rope.chars_at(from).next() else {
        return prev_word_end_from_end(rope, from, &category);
    };
    let head_start = from;
    let mut head = head_start;
    let mut prev_ch = first_char;
    let mut iter = rope.reversed_chars_at(from);

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (!ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

/// Like [`find_decimal_number_at`], but when the byte at `offset` is not a
/// digit, scans forward within the same line for the next ASCII digit and
/// returns the range of the number that begins there. Returns `None` when
/// no digit appears between `offset` and the next line ending.
pub fn find_decimal_number_seeking(rope: &Rope, offset: usize) -> Option<std::ops::Range<usize>> {
    if let Some(range) = find_decimal_number_at(rope, offset) {
        return Some(range);
    }
    let mut cursor = offset;
    for ch in rope.chars_at(offset) {
        if ch == '\n' || ch == '\r' {
            return None;
        }
        if ch.is_ascii_digit() {
            return find_decimal_number_at(rope, cursor);
        }
        cursor += ch.len_utf8();
    }
    None
}

/// Returns the byte range of the decimal number at `offset` in `rope`, or
/// `None` if the byte at `offset` is not an ASCII digit. The range spans the
/// run of digits and optionally a leading `-` when the `-` is preceded by
/// whitespace, the start of the rope, or a non-word character (so `-42`
/// reads as a signed number, but the `3` in `5-3` does not).
pub fn find_decimal_number_at(rope: &Rope, offset: usize) -> Option<std::ops::Range<usize>> {
    let head = rope.chars_at(offset).next()?;
    if !head.is_ascii_digit() {
        return None;
    }

    let mut start = offset;
    let iter = rope.reversed_chars_at(offset);
    for prev in iter {
        if !prev.is_ascii_digit() {
            break;
        }
        start -= prev.len_utf8();
    }

    let mut end = offset + head.len_utf8();
    let chars = rope.chars_at(end);
    for next in chars {
        if !next.is_ascii_digit() {
            break;
        }
        end += next.len_utf8();
    }

    if start > 0 {
        let minus_pos = start - 1;
        if let Some('-') = rope.reversed_chars_at(start).next() {
            let preceding = if minus_pos == 0 {
                None
            } else {
                rope.reversed_chars_at(minus_pos).next()
            };
            let signed = match preceding {
                None => true,
                Some(c) => !c.is_alphanumeric() && c != '_',
            };
            if signed {
                start = minus_pos;
            }
        }
    }

    Some(start..end)
}

fn prev_word_end_from_end<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    from: usize,
    category: &F,
) -> usize {
    let head_start = from;
    let mut head = head_start;
    let mut iter = rope.reversed_chars_at(from);
    let Some(seed) = iter.next() else {
        return head;
    };
    head -= seed.len_utf8();
    let mut prev_ch = seed;

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = category(prev_ch) != category(ch);
        let target = boundary && (!ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start - seed.len_utf8() {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn categorize_covers_main_classes() {
        assert_eq!(categorize_char('a'), CharCategory::Word);
        assert_eq!(categorize_char('1'), CharCategory::Word);
        assert_eq!(categorize_char('_'), CharCategory::Word);
        assert_eq!(categorize_char(' '), CharCategory::Whitespace);
        assert_eq!(categorize_char('\t'), CharCategory::Whitespace);
        assert_eq!(categorize_char('\n'), CharCategory::Eol);
        assert_eq!(categorize_char('.'), CharCategory::Punctuation);
        assert_eq!(categorize_char(','), CharCategory::Punctuation);
    }

    #[test]
    fn next_word_start_basic() {
        let r = rope("hello world");
        assert_eq!(next_word_start(&r, 0), 6);
        assert_eq!(next_word_start(&r, 6), 11);
    }

    #[test]
    fn next_word_start_from_whitespace_jumps_across_word() {
        let r = rope("hello world foo");
        assert_eq!(next_word_start(&r, 5), 12);
    }

    #[test]
    fn next_word_start_three_words() {
        let r = rope("abc def ghi");
        assert_eq!(next_word_start(&r, 0), 4);
        assert_eq!(next_word_start(&r, 4), 8);
        assert_eq!(next_word_start(&r, 8), 11);
    }

    #[test]
    fn next_word_start_at_end_is_noop() {
        let r = rope("hello");
        assert_eq!(next_word_start(&r, 5), 5);
    }

    #[test]
    fn next_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(next_word_start(&r, 0), 0);
    }

    #[test]
    fn next_word_start_underscore_is_word() {
        let r = rope("foo_bar baz");
        assert_eq!(next_word_start(&r, 0), 8);
    }

    #[test]
    fn next_word_start_punctuation_boundary() {
        let r = rope("alphanumeric.and");
        assert_eq!(next_word_start(&r, 0), 12);
        assert_eq!(next_word_start(&r, 12), 16);
    }

    #[test]
    fn next_word_start_punctuation_group_boundary() {
        let r = rope("alphanumeric.!,and");
        assert_eq!(next_word_start(&r, 0), 12);
        assert_eq!(next_word_start(&r, 12), 15);
    }

    #[test]
    fn next_word_start_stops_on_newline() {
        let r = rope("foo\nbar");
        assert_eq!(next_word_start(&r, 0), 3);
        assert_eq!(next_word_start(&r, 3), 7);
    }

    #[test]
    fn next_word_start_bridges_consecutive_newlines() {
        let r = rope("foo\n\nbar");
        assert_eq!(next_word_start(&r, 0), 3);
        assert_eq!(next_word_start(&r, 3), 5);
    }

    #[test]
    fn next_word_start_multibyte() {
        let r = rope("héllo wörld");
        let world_start = "héllo ".len();
        assert_eq!(next_word_start(&r, 0), world_start);
    }

    #[test]
    fn next_word_end_basic() {
        let r = rope("hello world");
        assert_eq!(next_word_end(&r, 0), 5);
    }

    #[test]
    fn next_word_end_from_whitespace() {
        let r = rope("hello world");
        assert_eq!(next_word_end(&r, 5), 11);
    }

    #[test]
    fn next_word_end_on_last_word_char_goes_to_next_word_end() {
        let r = rope("hello world foo");
        assert_eq!(next_word_end(&r, 10), 15);
    }

    #[test]
    fn next_word_end_at_end_is_noop() {
        let r = rope("hello");
        assert_eq!(next_word_end(&r, 5), 5);
    }

    #[test]
    fn next_word_end_empty_rope() {
        let r = rope("");
        assert_eq!(next_word_end(&r, 0), 0);
    }

    #[test]
    fn next_word_end_punctuation() {
        let r = rope("abc..def");
        assert_eq!(next_word_end(&r, 0), 3);
        assert_eq!(next_word_end(&r, 3), 5);
        assert_eq!(next_word_end(&r, 5), 8);
    }

    #[test]
    fn next_word_end_multibyte() {
        let r = rope("héllo wörld");
        let hello_end = "héllo".len();
        assert_eq!(next_word_end(&r, 0), hello_end);
    }

    #[test]
    fn prev_word_start_basic() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 6), 0);
        assert_eq!(prev_word_start(&r, 11), 6);
    }

    #[test]
    fn prev_word_start_from_whitespace() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 5), 0);
    }

    #[test]
    fn prev_word_start_from_end() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 11), 6);
    }

    #[test]
    fn prev_word_start_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(prev_word_start(&r, 0), 0);
    }

    #[test]
    fn prev_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(prev_word_start(&r, 0), 0);
    }

    #[test]
    fn prev_word_start_punctuation() {
        let r = rope("abc.def");
        assert_eq!(prev_word_start(&r, 7), 4);
        assert_eq!(prev_word_start(&r, 4), 3);
        assert_eq!(prev_word_start(&r, 3), 0);
    }

    #[test]
    fn prev_word_start_multibyte() {
        let r = rope("héllo wörld");
        let world_start = "héllo ".len();
        let rope_len = r.len();
        assert_eq!(prev_word_start(&r, rope_len), world_start);
        assert_eq!(prev_word_start(&r, world_start), 0);
    }

    #[test]
    fn prev_word_start_across_newline() {
        let r = rope("foo\nbar");
        assert_eq!(prev_word_start(&r, 7), 4);
        assert_eq!(prev_word_start(&r, 4), 0);
    }

    #[test]
    fn next_word_start_trailing_whitespace() {
        let r = rope("hello   ");
        assert_eq!(next_word_start(&r, 0), 8);
    }

    #[test]
    fn prev_word_end_basic() {
        let r = rope("hello world");
        assert_eq!(prev_word_end(&r, 9), 5);
    }

    #[test]
    fn prev_word_end_from_end() {
        let r = rope("hello world");
        assert_eq!(prev_word_end(&r, 11), 5);
    }

    #[test]
    fn prev_word_end_from_word_start() {
        let r = rope("foo bar");
        assert_eq!(prev_word_end(&r, 4), 3);
    }

    #[test]
    fn prev_word_end_from_whitespace_skips_prev_word() {
        let r = rope("foo bar baz");
        assert_eq!(prev_word_end(&r, 7), 3);
    }

    #[test]
    fn prev_word_end_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(prev_word_end(&r, 0), 0);
    }

    #[test]
    fn prev_word_end_empty_rope() {
        let r = rope("");
        assert_eq!(prev_word_end(&r, 0), 0);
    }

    #[test]
    fn prev_word_end_punctuation() {
        let r = rope("abc.def");
        assert_eq!(prev_word_end(&r, 7), 4);
        assert_eq!(prev_word_end(&r, 4), 3);
    }

    #[test]
    fn prev_word_end_multibyte() {
        let r = rope("héllo wörld");
        let world_end = r.len();
        let hello_end = "héllo".len();
        assert_eq!(prev_word_end(&r, world_end), hello_end);
    }

    #[test]
    fn prev_word_end_all_newlines() {
        let r = rope("\n\n\n\n\n");
        assert_eq!(prev_word_end(&r, 5), 0);
    }

    #[test]
    fn next_long_word_start_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        assert_eq!(next_long_word_start(&r, 0), 8);
        assert_eq!(next_word_start(&r, 0), 3);
    }

    #[test]
    fn next_long_word_start_chained_punctuation() {
        let r = rope("a!@b cd ef");
        assert_eq!(next_long_word_start(&r, 0), 5);
        assert_eq!(next_long_word_start(&r, 5), 8);
    }

    #[test]
    fn next_long_word_start_stops_on_newline() {
        let r = rope("foo.bar\nbaz");
        assert_eq!(next_long_word_start(&r, 0), 7);
        assert_eq!(next_long_word_start(&r, 7), 11);
    }

    #[test]
    fn next_long_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(next_long_word_start(&r, 0), 0);
    }

    #[test]
    fn next_long_word_end_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        assert_eq!(next_long_word_end(&r, 0), 7);
        assert_eq!(next_word_end(&r, 0), 3);
    }

    #[test]
    fn next_long_word_end_chained_punctuation() {
        let r = rope("a!@b cd");
        assert_eq!(next_long_word_end(&r, 0), 4);
    }

    #[test]
    fn next_long_word_end_multibyte() {
        let r = rope("foo.héllo wörld");
        let hello_end = "foo.héllo".len();
        assert_eq!(next_long_word_end(&r, 0), hello_end);
    }

    #[test]
    fn prev_long_word_start_treats_punctuation_as_word() {
        let r = rope("foo bar.baz");
        assert_eq!(prev_long_word_start(&r, 11), 4);
        assert_eq!(prev_word_start(&r, 11), 8);
    }

    #[test]
    fn prev_long_word_start_chained_punctuation() {
        let r = rope("ab cd ef!@g");
        let len = r.len();
        assert_eq!(prev_long_word_start(&r, len), 6);
    }

    #[test]
    fn prev_long_word_start_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(prev_long_word_start(&r, 0), 0);
    }

    #[test]
    fn prev_long_word_end_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        let len = r.len();
        assert_eq!(prev_long_word_end(&r, len), 7);
        assert_eq!(prev_word_end(&r, len), 7);
    }

    #[test]
    fn prev_long_word_end_skips_internal_punctuation_boundary() {
        let r = rope("aa bb.cc dd");
        assert_eq!(prev_long_word_end(&r, 6), 2);
        assert_eq!(prev_word_end(&r, 6), 5);
    }

    #[test]
    fn find_decimal_at_returns_digit_run_when_cursor_on_digit() {
        let r = rope("foo 123 bar");
        assert_eq!(find_decimal_number_at(&r, 4), Some(4..7));
        assert_eq!(find_decimal_number_at(&r, 5), Some(4..7));
        assert_eq!(find_decimal_number_at(&r, 6), Some(4..7));
    }

    #[test]
    fn find_decimal_at_returns_none_when_cursor_off_digit() {
        let r = rope("foo 123 bar");
        assert_eq!(find_decimal_number_at(&r, 0), None);
        assert_eq!(find_decimal_number_at(&r, 3), None);
        assert_eq!(find_decimal_number_at(&r, 7), None);
    }

    #[test]
    fn find_decimal_at_includes_leading_minus_when_isolated() {
        let r = rope("-42");
        assert_eq!(find_decimal_number_at(&r, 1), Some(0..3));
    }

    #[test]
    fn find_decimal_at_includes_minus_after_whitespace() {
        let r = rope("count: -42");
        assert_eq!(find_decimal_number_at(&r, 8), Some(7..10));
    }

    #[test]
    fn find_decimal_at_excludes_minus_after_alphanumeric() {
        let r = rope("5-3");
        assert_eq!(find_decimal_number_at(&r, 2), Some(2..3));
    }

    #[test]
    fn find_decimal_at_excludes_minus_after_word_char() {
        let r = rope("var-42");
        assert_eq!(find_decimal_number_at(&r, 4), Some(4..6));
    }

    #[test]
    fn find_decimal_at_includes_minus_after_punctuation() {
        let r = rope("(-42)");
        assert_eq!(find_decimal_number_at(&r, 2), Some(1..4));
    }

    #[test]
    fn find_decimal_at_at_start_of_rope() {
        let r = rope("42 abc");
        assert_eq!(find_decimal_number_at(&r, 0), Some(0..2));
    }

    #[test]
    fn find_decimal_at_empty_rope() {
        let r = rope("");
        assert_eq!(find_decimal_number_at(&r, 0), None);
    }

    #[test]
    fn find_decimal_seeking_uses_existing_match_when_on_digit() {
        let r = rope("foo 123 bar");
        assert_eq!(find_decimal_number_seeking(&r, 4), Some(4..7));
        assert_eq!(find_decimal_number_seeking(&r, 6), Some(4..7));
    }

    #[test]
    fn find_decimal_seeking_jumps_forward_to_next_digit() {
        let r = rope("let x = 42");
        assert_eq!(find_decimal_number_seeking(&r, 0), Some(8..10));
        assert_eq!(find_decimal_number_seeking(&r, 6), Some(8..10));
        assert_eq!(find_decimal_number_seeking(&r, 7), Some(8..10));
    }

    #[test]
    fn find_decimal_seeking_picks_first_digit_when_multiple() {
        let r = rope("a 5 b 7");
        assert_eq!(find_decimal_number_seeking(&r, 0), Some(2..3));
    }

    #[test]
    fn find_decimal_seeking_no_op_after_last_digit_on_line() {
        let r = rope("42 abc");
        assert_eq!(find_decimal_number_seeking(&r, 3), None);
    }

    #[test]
    fn find_decimal_seeking_no_op_when_line_has_no_digit() {
        let r = rope("abcdef");
        assert_eq!(find_decimal_number_seeking(&r, 0), None);
    }

    #[test]
    fn find_decimal_seeking_does_not_cross_newline() {
        let r = rope("abc\n42");
        assert_eq!(find_decimal_number_seeking(&r, 0), None);
    }

    #[test]
    fn find_decimal_seeking_picks_signed_minus_when_present() {
        let r = rope("let x = -42");
        assert_eq!(find_decimal_number_seeking(&r, 6), Some(8..11));
    }

    #[test]
    fn long_word_category_collapses_word_and_punctuation() {
        assert_eq!(long_word_category('a'), CharCategory::Word);
        assert_eq!(long_word_category('.'), CharCategory::Word);
        assert_eq!(long_word_category('!'), CharCategory::Word);
        assert_eq!(long_word_category(' '), CharCategory::Whitespace);
        assert_eq!(long_word_category('\n'), CharCategory::Eol);
    }
}
