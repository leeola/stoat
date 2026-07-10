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
    next_word_start_with(rope, from, from, categorize_char).1
}

pub fn next_long_word_start(rope: &Rope, from: usize) -> usize {
    next_word_start_with(rope, from, from, long_word_category).1
}

/// [`next_word_start`] as a Helix `range_to_target` step: given the origin
/// `(anchor, head)`, returns the new `(anchor, head)`. The anchor advances past
/// a leading newline run and past a single leading boundary char so a forward
/// word motion from whitespace or a blank line does not select the gap.
pub fn next_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_start_with(rope, anchor, head, categorize_char)
}

pub fn next_long_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_start_with(rope, anchor, head, long_word_category)
}

fn next_word_start_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: F,
) -> (usize, usize) {
    forward_word_range(rope, anchor_in, from, &category, |_prev, ch, boundary| {
        boundary && (char_is_line_ending(ch) || !ch.is_whitespace())
    })
}

pub fn next_word_end(rope: &Rope, from: usize) -> usize {
    next_word_end_with(rope, from, from, categorize_char).1
}

pub fn next_long_word_end(rope: &Rope, from: usize) -> usize {
    next_word_end_with(rope, from, from, long_word_category).1
}

pub fn next_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_end_with(rope, anchor, head, categorize_char)
}

pub fn next_long_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_end_with(rope, anchor, head, long_word_category)
}

fn next_word_end_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: F,
) -> (usize, usize) {
    forward_word_range(rope, anchor_in, from, &category, |prev, ch, boundary| {
        boundary && (!prev.is_whitespace() || char_is_line_ending(ch))
    })
}

/// Shared forward word-motion scan matching Helix's `range_to_target`. Scans
/// from `from`, returning the target `head` and an `anchor` that starts at
/// `anchor_in` and advances past a leading newline run (so the head runs through
/// a blank line) and past a single leading boundary char (so the first target
/// boundary at `head_start` moves the anchor onto the span start).
fn forward_word_range<C, T>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: C,
    is_target: T,
) -> (usize, usize)
where
    C: Fn(char) -> CharCategory,
    T: Fn(char, char, bool) -> bool,
{
    let mut chars = rope.chars_at(from).peekable();
    let Some(first_char) = chars.next() else {
        return (anchor_in, from);
    };

    let mut anchor = anchor_in;
    let mut head = from + first_char.len_utf8();
    let mut prev_ch = first_char;

    if char_is_line_ending(first_char) {
        while let Some(&ch) = chars.peek() {
            if !char_is_line_ending(ch) {
                break;
            }
            chars.next();
            head += ch.len_utf8();
            prev_ch = ch;
        }
        anchor = head;
    }

    let head_start = head;
    loop {
        let Some(ch) = chars.next() else {
            return (anchor, head);
        };
        let boundary = category(prev_ch) != category(ch);
        if is_target(prev_ch, ch, boundary) {
            if head == head_start {
                anchor = head;
            } else {
                return (anchor, head);
            }
        }
        prev_ch = ch;
        head += ch.len_utf8();
    }
}

pub fn prev_word_start(rope: &Rope, from: usize) -> usize {
    prev_word_start_with(rope, from, from, categorize_char).1
}

pub fn prev_long_word_start(rope: &Rope, from: usize) -> usize {
    prev_word_start_with(rope, from, from, long_word_category).1
}

/// [`prev_word_start`] as a range_to_target step: given the origin
/// `(anchor, head)`, returns the new `(anchor, head)`. The anchor retreats past
/// a trailing newline run and past a single trailing boundary char, so a
/// backward word motion from whitespace or after a boundary does not keep the
/// gap in the selection.
pub fn prev_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_start_with(rope, anchor, head, categorize_char)
}

pub fn prev_long_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_start_with(rope, anchor, head, long_word_category)
}

fn prev_word_start_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: F,
) -> (usize, usize) {
    backward_word_range(rope, anchor_in, from, &category, |prev, ch, boundary| {
        boundary && (!prev.is_whitespace() || char_is_line_ending(ch))
    })
}

/// Shared backward word-motion scan, the reverse mirror of
/// [`forward_word_range`]. Scans from `from` toward the buffer start, returning
/// the target `head` and an `anchor` that starts at `anchor_in` and retreats
/// past a trailing newline run (so the head runs back through a blank line) and
/// past a single trailing boundary char (so the first target boundary at
/// `head_start` moves the anchor onto the span end).
fn backward_word_range<C, T>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: C,
    is_target: T,
) -> (usize, usize)
where
    C: Fn(char) -> CharCategory,
    T: Fn(char, char, bool) -> bool,
{
    if from == 0 {
        return (anchor_in, 0);
    }

    let mut iter = rope.reversed_chars_at(from).peekable();
    let (mut head, mut prev_ch) = match rope.chars_at(from).next() {
        Some(seed) => (from, seed),
        None => match iter.next() {
            Some(seed) => (from - seed.len_utf8(), seed),
            None => return (anchor_in, from),
        },
    };
    let mut anchor = anchor_in;

    if iter.peek().is_some_and(|&ch| char_is_line_ending(ch)) {
        while let Some(&ch) = iter.peek() {
            if !char_is_line_ending(ch) {
                break;
            }
            iter.next();
            head -= ch.len_utf8();
            prev_ch = ch;
        }
        anchor = head;
    }

    let head_start = head;
    loop {
        let Some(ch) = iter.next() else {
            return (anchor, head);
        };
        let boundary = category(prev_ch) != category(ch);
        if is_target(prev_ch, ch, boundary) {
            if head == head_start {
                anchor = head;
            } else {
                return (anchor, head);
            }
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

pub fn prev_word_end(rope: &Rope, from: usize) -> usize {
    prev_word_end_with(rope, from, from, categorize_char).1
}

pub fn prev_long_word_end(rope: &Rope, from: usize) -> usize {
    prev_word_end_with(rope, from, from, long_word_category).1
}

pub fn prev_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_end_with(rope, anchor, head, categorize_char)
}

pub fn prev_long_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_end_with(rope, anchor, head, long_word_category)
}

fn prev_word_end_with<F: Fn(char) -> CharCategory>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    category: F,
) -> (usize, usize) {
    backward_word_range(rope, anchor_in, from, &category, |_prev, ch, boundary| {
        boundary && (!ch.is_whitespace() || char_is_line_ending(ch))
    })
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

/// Classification of a number literal recognised by [`find_number_at`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NumberKind {
    Decimal,
    Hex,
    Binary,
    Octal,
}

impl NumberKind {
    pub fn radix(self) -> u32 {
        match self {
            NumberKind::Decimal => 10,
            NumberKind::Hex => 16,
            NumberKind::Binary => 2,
            NumberKind::Octal => 8,
        }
    }
}

/// A number literal found in a [`Rope`]: byte range plus its category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberMatch {
    pub range: std::ops::Range<usize>,
    pub kind: NumberKind,
}

/// Returns the literal at `offset` -- a `0x`/`0X` hex, `0b`/`0B` binary,
/// `0o`/`0O` octal literal, or a decimal number (with optional leading
/// `-`). Underscore separators inside the body (`0xff_ff_ff_ff`) are
/// accepted; the caller is expected to regroup on emit (see
/// `compute_number_delta`). Trailing `_`s are excluded from the
/// captured range. Falls through to [`find_decimal_number_at`] when
/// the surrounding text does not form a radix literal.
pub fn find_number_at(rope: &Rope, offset: usize) -> Option<NumberMatch> {
    match find_radix_literal_at(rope, offset) {
        RadixResult::Match(m) => Some(m),
        RadixResult::Rejected => None,
        RadixResult::NoRadix => {
            let range = find_decimal_number_at(rope, offset)?;
            Some(NumberMatch {
                range,
                kind: NumberKind::Decimal,
            })
        },
    }
}

enum RadixResult {
    NoRadix,
    Rejected,
    Match(NumberMatch),
}

fn find_radix_literal_at(rope: &Rope, offset: usize) -> RadixResult {
    let Some(head) = rope.chars_at(offset).next() else {
        return RadixResult::NoRadix;
    };
    let head_potential = head == '0'
        || head == '_'
        || matches!(head, 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
        || head.is_ascii_hexdigit();
    if !head_potential {
        return RadixResult::NoRadix;
    }

    let mut start = offset;
    for prev in rope.reversed_chars_at(offset) {
        if prev == '_'
            || prev.is_ascii_hexdigit()
            || matches!(prev, 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
        {
            start -= prev.len_utf8();
            continue;
        }
        break;
    }

    let mut prefix_iter = rope.chars_at(start);
    let Some(zero) = prefix_iter.next() else {
        return RadixResult::NoRadix;
    };
    let Some(marker) = prefix_iter.next() else {
        return RadixResult::NoRadix;
    };
    if zero != '0' {
        return RadixResult::NoRadix;
    }
    let kind = match marker {
        'x' | 'X' => NumberKind::Hex,
        'b' | 'B' => NumberKind::Binary,
        'o' | 'O' => NumberKind::Octal,
        _ => return RadixResult::NoRadix,
    };

    let body_start = start + zero.len_utf8() + marker.len_utf8();
    let mut body_end = body_start;
    let mut last_digit_end = body_start;
    let mut saw_digit = false;
    let radix = kind.radix();
    for ch in rope.chars_at(body_start) {
        if ch == '_' {
            body_end += ch.len_utf8();
            continue;
        }
        if !ch.is_digit(radix) {
            break;
        }
        body_end += ch.len_utf8();
        last_digit_end = body_end;
        saw_digit = true;
    }

    if !saw_digit {
        return RadixResult::Rejected;
    }

    let body_end = last_digit_end;

    if offset < start || offset >= body_end {
        return RadixResult::NoRadix;
    }

    RadixResult::Match(NumberMatch {
        range: start..body_end,
        kind,
    })
}

/// Like [`find_number_at`], but when nothing is found at `offset`, scans
/// forward within the same line for the next digit and tries again.
/// Never crosses a line ending.
pub fn find_number_seeking(rope: &Rope, offset: usize) -> Option<NumberMatch> {
    if let Some(m) = find_number_at(rope, offset) {
        return Some(m);
    }
    let mut cursor = offset;
    for ch in rope.chars_at(offset) {
        if ch == '\n' || ch == '\r' {
            return None;
        }
        if ch.is_ascii_digit() {
            return find_number_at(rope, cursor);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn next_word_start_range_advances_anchor_like_helix() {
        // Mirrors Helix's move_next_word_start, called as the handler does
        // (anchor == head == seed) so the block-cursor prep already holds. Each
        // string is short enough to read every offset directly.
        let cases: &[(&str, usize, (usize, usize))] = &[
            // On a word start: anchor stays, head reaches the next word.
            ("ab cd", 0, (0, 3)),
            // Mid-word: anchor stays at the seed.
            ("hello", 2, (2, 5)),
            // One leading space (a single boundary char): anchor advances past
            // it and the head runs through the first word to the next.
            (" ab cd", 0, (1, 4)),
            // A multi-space run: anchor stays, head stops at the word start.
            ("  ab cd", 0, (0, 2)),
            // A leading newline run: anchor skips it, head runs through the word.
            ("\n\nab cd", 0, (2, 5)),
        ];
        for (text, seed, expected) in cases {
            assert_eq!(
                next_word_start_range(&rope(text), *seed, *seed),
                *expected,
                "next_word_start on {text:?} from {seed}"
            );
        }
    }

    #[test]
    fn prev_word_start_range_retreats_anchor_like_helix() {
        // Mirror of the forward table for backward `b`, called as the handler
        // does with the origin anchor at the block edge (seed + 1) and the head
        // at the seed. `(text, anchor_in, seed, expected)`.
        let cases: &[(&str, usize, usize, (usize, usize))] = &[
            // On a mid-word char, the anchor stays at the block edge.
            ("ab cd", 5, 4, (5, 3)),
            // On a word start, the anchor retreats onto the seed, excluding it.
            ("ab cd", 4, 3, (3, 0)),
            // A trailing newline run retreats the anchor past it.
            ("ab\n\ncd", 5, 4, (2, 0)),
        ];
        for (text, anchor_in, seed, expected) in cases {
            assert_eq!(
                prev_word_start_range(&rope(text), *anchor_in, *seed),
                *expected,
                "prev_word_start on {text:?} from anchor {anchor_in} head {seed}"
            );
        }
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
        // From inside the blank run the head bridges both newlines and runs
        // through the following word to its end, matching Helix.
        assert_eq!(next_word_start(&r, 3), 8);
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
    fn find_number_at_recognises_hex_literal_from_each_position() {
        let r = rope("0xff");
        for offset in 0..4 {
            assert_eq!(
                find_number_at(&r, offset),
                Some(NumberMatch {
                    range: 0..4,
                    kind: NumberKind::Hex
                }),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn find_number_at_recognises_uppercase_hex_marker() {
        let r = rope("0XFF");
        assert_eq!(
            find_number_at(&r, 1),
            Some(NumberMatch {
                range: 0..4,
                kind: NumberKind::Hex
            })
        );
    }

    #[test]
    fn find_number_at_recognises_binary_literal() {
        let r = rope("0b1010");
        assert_eq!(
            find_number_at(&r, 3),
            Some(NumberMatch {
                range: 0..6,
                kind: NumberKind::Binary
            })
        );
    }

    #[test]
    fn find_number_at_recognises_octal_literal() {
        let r = rope("0o17");
        assert_eq!(
            find_number_at(&r, 2),
            Some(NumberMatch {
                range: 0..4,
                kind: NumberKind::Octal
            })
        );
    }

    #[test]
    fn find_number_at_falls_through_to_decimal() {
        let r = rope("foo 42 bar");
        assert_eq!(
            find_number_at(&r, 4),
            Some(NumberMatch {
                range: 4..6,
                kind: NumberKind::Decimal
            })
        );
    }

    #[test]
    fn find_number_at_accepts_underscored_hex_literal() {
        let r = rope("0xff_ff");
        let expected = Some(NumberMatch {
            range: 0..7,
            kind: NumberKind::Hex,
        });
        for offset in [0, 2, 4, 5, 6] {
            assert_eq!(find_number_at(&r, offset), expected, "offset {offset}");
        }
    }

    #[test]
    fn find_number_at_accepts_underscored_binary_literal() {
        let r = rope("0b1010_1010_1010");
        assert_eq!(
            find_number_at(&r, 9),
            Some(NumberMatch {
                range: 0..16,
                kind: NumberKind::Binary,
            })
        );
    }

    #[test]
    fn find_number_at_excludes_trailing_underscore_from_range() {
        let r = rope("0xff_ ");
        assert_eq!(
            find_number_at(&r, 2),
            Some(NumberMatch {
                range: 0..4,
                kind: NumberKind::Hex,
            })
        );
        assert_eq!(find_number_at(&r, 4), None);
    }

    #[test]
    fn find_number_at_rejects_radix_with_no_digits() {
        let r = rope("0x_");
        assert_eq!(find_number_at(&r, 0), None);
        assert_eq!(find_number_at(&r, 2), None);
    }

    #[test]
    fn find_number_at_rejects_hex_without_prefix() {
        let r = rope("foo abcdef bar");
        assert_eq!(find_number_at(&r, 4), None);
    }

    #[test]
    fn find_number_at_rejects_when_outside_body() {
        let r = rope("0b10ab");
        assert_eq!(find_number_at(&r, 4), None);
    }

    #[test]
    fn find_number_at_isolated_in_surrounding_text() {
        let r = rope("(0xff)");
        assert_eq!(
            find_number_at(&r, 3),
            Some(NumberMatch {
                range: 1..5,
                kind: NumberKind::Hex
            })
        );
    }

    #[test]
    fn find_number_seeking_jumps_to_hex_literal() {
        let r = rope("let x = 0xff");
        assert_eq!(
            find_number_seeking(&r, 0),
            Some(NumberMatch {
                range: 8..12,
                kind: NumberKind::Hex
            })
        );
    }

    #[test]
    fn find_number_seeking_does_not_cross_newline() {
        let r = rope("foo\n0xff");
        assert_eq!(find_number_seeking(&r, 0), None);
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
