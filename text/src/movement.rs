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

/// Word-motion targets for [`word_move_range`], covering both the
/// regular (`categorize_char`) and long-word (`long_word_category`)
/// boundary families.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WordTarget {
    NextStart,
    NextEnd,
    PrevStart,
    PrevEnd,
    NextLongStart,
    NextLongEnd,
    PrevLongStart,
    PrevLongEnd,
}

impl WordTarget {
    fn is_prev(self) -> bool {
        matches!(
            self,
            WordTarget::PrevStart
                | WordTarget::PrevEnd
                | WordTarget::PrevLongStart
                | WordTarget::PrevLongEnd
        )
    }

    fn category(self) -> fn(char) -> CharCategory {
        if matches!(
            self,
            WordTarget::NextLongStart
                | WordTarget::NextLongEnd
                | WordTarget::PrevLongStart
                | WordTarget::PrevLongEnd
        ) {
            long_word_category
        } else {
            categorize_char
        }
    }

    /// True when the target lands on the first char of a run (a
    /// "start"-family motion); false for "end"-family motions. Mirrors
    /// the two predicate shapes in Helix's `reached_target`.
    fn lands_on_run_start(self) -> bool {
        matches!(
            self,
            WordTarget::NextStart
                | WordTarget::PrevEnd
                | WordTarget::NextLongStart
                | WordTarget::PrevLongEnd
        )
    }

    /// True for the one motion whose stored head stays on the last char
    /// of the region rather than the run boundary `word_move_range`
    /// reports: `PrevEnd` produces a reversed selection, whose block
    /// cursor is the head itself, so the head must land on the char.
    /// Forward motions store the boundary head and let the paint site
    /// derive the cursor via `cursor_offset`.
    fn shifts_head(self) -> bool {
        matches!(self, WordTarget::PrevEnd)
    }
}

/// Resolve a word motion to a `(anchor, head)` byte-offset range using
/// Helix's block-cursor semantics: the range is first collapsed to the
/// 1-grapheme cursor at `head` (the incoming `anchor` only selects the
/// collapse direction), then the head advances to the target and the
/// anchor is re-derived from the motion. This is what stops a repeated
/// `w`/`b` from dragging the stray boundary char the previous press
/// left under the cursor.
///
/// Offsets are rope byte offsets; the returned `head` sits on the run
/// boundary (one past the last consumed char for forward motions), so
/// callers that render a block cursor on the last char shift it back
/// themselves.
pub fn word_move_range(
    rope: &Rope,
    anchor: usize,
    head: usize,
    target: WordTarget,
    count: usize,
) -> (usize, usize) {
    let is_prev = target.is_prev();
    if (is_prev && head == 0) || (!is_prev && head == rope.len()) {
        return (anchor, head);
    }

    let prev_boundary = head - char_len_before(rope, head);
    let next_boundary = head + char_len_at(rope, head);
    let (mut a, mut h) = if is_prev {
        if anchor < head {
            (head, prev_boundary)
        } else {
            (next_boundary, head)
        }
    } else if anchor < head {
        (prev_boundary, head)
    } else {
        (head, next_boundary)
    };

    for _ in 0..count {
        let (na, nh) = range_to_target(rope, target, a, h);
        if (na, nh) == (a, h) {
            break;
        }
        a = na;
        h = nh;
    }
    (a, h)
}

/// Convert a stoat selection `(start, end, reversed)` through a word
/// motion into the new selection's `(tail, head)` byte offsets, or
/// `None` when the motion does not move. `head` sits on the run
/// boundary (one past the last consumed char for forward motions);
/// paint sites derive the block-cursor cell from it via `cursor_offset`.
/// `tail` is the anchor re-derived by [`word_move_range`], which is what
/// stops repeated motions from dragging a stray boundary char. For an
/// extend motion the caller keeps its own fixed tail and uses only
/// `head`.
pub fn word_selection_offsets(
    rope: &Rope,
    start: usize,
    end: usize,
    reversed: bool,
    target: WordTarget,
    count: usize,
) -> Option<(usize, usize)> {
    let head = if reversed { start } else { end };
    let collapsed = start == end;
    // Only the sign of `anchor_in - head` matters: it selects the
    // 1-grapheme collapse direction in `word_move_range`.
    let anchor_in = if !reversed && !collapsed {
        head.saturating_sub(1)
    } else {
        head
    };
    // The anchor is re-derived from the starting cursor and is
    // independent of count, so take it from a single step. The head
    // accumulates across all `count` words, preserving stoat's
    // count-spanning selection.
    let (tail, first_head) = word_move_range(rope, anchor_in, head, target, 1);
    if (tail, first_head) == (anchor_in, head) {
        return None;
    }
    let head_boundary = if count <= 1 {
        first_head
    } else {
        word_move_range(rope, anchor_in, head, target, count).1
    };
    let new_head = if target.shifts_head() {
        head_boundary - char_len_before(rope, head_boundary)
    } else {
        head_boundary
    };
    Some((tail, new_head))
}

fn range_to_target(rope: &Rope, target: WordTarget, anchor: usize, head: usize) -> (usize, usize) {
    let is_prev = target.is_prev();
    let mut anchor = anchor;
    let mut head = head;

    let mut prev_ch = if is_prev {
        rope.chars_at(head).next()
    } else {
        rope.reversed_chars_at(head).next()
    };

    while let Some(ch) = dir_char(rope, head, is_prev) {
        if char_is_line_ending(ch) {
            prev_ch = Some(ch);
            head = dir_advance(head, ch, is_prev);
        } else {
            break;
        }
    }
    if prev_ch.map(char_is_line_ending).unwrap_or(false) {
        anchor = head;
    }

    let head_start = head;
    while let Some(next_ch) = dir_char(rope, head, is_prev) {
        let reached = match prev_ch {
            None => true,
            Some(p) => reached_target(target, p, next_ch),
        };
        if reached {
            if head == head_start {
                anchor = head;
            } else {
                break;
            }
        }
        prev_ch = Some(next_ch);
        head = dir_advance(head, next_ch, is_prev);
    }
    (anchor, head)
}

fn reached_target(target: WordTarget, prev_ch: char, next_ch: char) -> bool {
    let category = target.category();
    let boundary = category(prev_ch) != category(next_ch);
    if target.lands_on_run_start() {
        boundary && (char_is_line_ending(next_ch) || !next_ch.is_whitespace())
    } else {
        boundary && (!prev_ch.is_whitespace() || char_is_line_ending(next_ch))
    }
}

fn dir_char(rope: &Rope, head: usize, is_prev: bool) -> Option<char> {
    if is_prev {
        rope.reversed_chars_at(head).next()
    } else {
        rope.chars_at(head).next()
    }
}

fn dir_advance(head: usize, ch: char, is_prev: bool) -> usize {
    if is_prev {
        head - ch.len_utf8()
    } else {
        head + ch.len_utf8()
    }
}

pub(crate) fn char_len_at(rope: &Rope, offset: usize) -> usize {
    rope.chars_at(offset)
        .next()
        .map(char::len_utf8)
        .unwrap_or(0)
}

pub(crate) fn char_len_before(rope: &Rope, offset: usize) -> usize {
    rope.reversed_chars_at(offset)
        .next()
        .map(char::len_utf8)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(count, (begin_anchor, begin_head), (expected_anchor, expected_head))`.
    type WordMoveCase = (usize, (usize, usize), (usize, usize));
    type WordMoveScenarios = &'static [(&'static str, &'static [WordMoveCase])];

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

    /// Ground-truth cases ported from Helix's
    /// `test_behaviour_when_moving_to_start_of_next_words`
    /// (`helix-core/src/movement.rs`). Each entry is
    /// `(count, (begin_anchor, begin_head), (expected_anchor, expected_head))`;
    /// all samples are ASCII so char indices equal byte offsets. Proves
    /// the block-cursor anchor re-derivation matches Helix.
    #[test]
    fn word_move_range_matches_helix_next_word_start() {
        let tests: WordMoveScenarios = &[
            (
                "Basic forward motion stops at the first space",
                &[(1, (0, 0), (0, 6))],
            ),
            (
                " Starting from a boundary advances the anchor",
                &[(1, (0, 0), (1, 10))],
            ),
            (
                "Long       whitespace gap is bridged by the head",
                &[(1, (0, 0), (0, 11))],
            ),
            (
                "Previous anchor is irrelevant for forward motions",
                &[(1, (12, 0), (0, 9))],
            ),
            (
                "    Starting from whitespace moves to last space in sequence",
                &[(1, (0, 0), (0, 4))],
            ),
            (
                "Starting from mid-word leaves anchor at start position and moves head",
                &[(1, (3, 3), (3, 9))],
            ),
            (
                "Identifiers_with_underscores are considered a single word",
                &[(1, (0, 0), (0, 29))],
            ),
            (
                "alphanumeric.!,and.?=punctuation are considered 'words' for word motion",
                &[
                    (1, (0, 0), (0, 12)),
                    (1, (0, 12), (12, 15)),
                    (1, (12, 15), (15, 18)),
                ],
            ),
            (
                "...   ... punctuation and spaces behave as expected",
                &[(1, (0, 0), (0, 6)), (1, (0, 6), (6, 10))],
            ),
            (
                ".._.._ punctuation is not joined by underscores into a single block",
                &[(1, (0, 0), (0, 2))],
            ),
            (
                "Multiple motions at once resolve correctly",
                &[(3, (0, 0), (17, 20))],
            ),
            (
                "Excessive motions are performed partially",
                &[(999, (0, 0), (32, 41))],
            ),
        ];
        for (sample, scenario) in tests {
            let r = rope(sample);
            for &(count, (ba, bh), (ea, eh)) in *scenario {
                assert_eq!(
                    word_move_range(&r, ba, bh, WordTarget::NextStart, count),
                    (ea, eh),
                    "sample {sample:?} from ({ba},{bh}) x{count}",
                );
            }
        }
    }

    #[test]
    fn word_move_range_treats_scope_operator_as_one_run() {
        let r = rope("foo::bar");
        // `w` from the start of `foo` lands on the start of `::`.
        assert_eq!(word_move_range(&r, 0, 0, WordTarget::NextStart, 1), (0, 3));
        // and the next `w` selects the whole `::` run, head on `bar`.
        assert_eq!(word_move_range(&r, 0, 3, WordTarget::NextStart, 1), (3, 5));

        let r = rope("a::b::c");
        // The leading 1-char word `a` is followed immediately by a
        // boundary, so the anchor advances past it (Helix's "advancing
        // the anchor" rule); three motions land the head on the final
        // `c` at offset 6.
        assert_eq!(word_move_range(&r, 0, 0, WordTarget::NextStart, 3), (4, 6));
    }

    /// `NextEnd` cases ported from Helix's
    /// `test_behaviour_when_moving_to_end_of_next_words`.
    #[test]
    fn word_move_range_matches_helix_next_word_end() {
        let tests: WordMoveScenarios = &[
            (
                "Basic forward motion from the start of a word to the end of it",
                &[(1, (0, 0), (0, 5))],
            ),
            (
                "Basic forward motion from the end of a word to the end of the next",
                &[(1, (0, 5), (5, 13))],
            ),
            (
                "Basic forward motion from the middle of a word to the end of it",
                &[(1, (2, 2), (2, 5))],
            ),
            (
                "Previous anchor is irrelevant for end of word motion",
                &[(1, (12, 2), (2, 8))],
            ),
            (
                "alphanumeric.!,and.?=punctuation are considered 'words' for word motion",
                &[(1, (0, 0), (0, 12)), (1, (0, 12), (12, 15))],
            ),
        ];
        for (sample, scenario) in tests {
            let r = rope(sample);
            for &(count, (ba, bh), (ea, eh)) in *scenario {
                assert_eq!(
                    word_move_range(&r, ba, bh, WordTarget::NextEnd, count),
                    (ea, eh),
                    "sample {sample:?} from ({ba},{bh}) x{count}",
                );
            }
        }
    }

    /// `PrevStart` cases ported from Helix's
    /// `test_behaviour_when_moving_to_start_of_previous_words`.
    #[test]
    fn word_move_range_matches_helix_prev_word_start() {
        let tests: WordMoveScenarios = &[
            (
                "Basic backward motion from the middle of a word",
                &[(1, (3, 3), (4, 0))],
            ),
            (
                "Previous anchor is irrelevant for backward motions",
                &[(1, (12, 5), (6, 0))],
            ),
            (
                "    Starting from whitespace moves to first space in sequence",
                &[(1, (0, 4), (4, 0))],
            ),
            (
                "alphanumeric.!,and.?=punctuation are considered 'words' for word motion",
                &[(1, (29, 30), (30, 21)), (1, (30, 21), (21, 18))],
            ),
        ];
        for (sample, scenario) in tests {
            let r = rope(sample);
            for &(count, (ba, bh), (ea, eh)) in *scenario {
                assert_eq!(
                    word_move_range(&r, ba, bh, WordTarget::PrevStart, count),
                    (ea, eh),
                    "sample {sample:?} from ({ba},{bh}) x{count}",
                );
            }
        }
    }
}
