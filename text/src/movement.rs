use crate::Rope;
use unicode_general_category::{get_general_category, GeneralCategory};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CharCategory {
    Whitespace,
    Eol,
    Word,
    Punctuation,
    Unknown,
}

/// The class a character belongs to, which the short-word (`w`/`b`/`e`)
/// motions and the word text objects both break on wherever two neighbours
/// differ.
///
/// `Unknown` is the class nothing else claimed, so a run of such characters
/// still groups together and a `w` stops where it meets anything named.
pub fn categorize_char(ch: char) -> CharCategory {
    if char_is_line_ending(ch) {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else if char_is_word(ch) {
        CharCategory::Word
    } else if char_is_punctuation(ch) {
        CharCategory::Punctuation
    } else {
        CharCategory::Unknown
    }
}

/// Whether `ch` is punctuation for the purpose of word motions.
///
/// The Unicode general categories for punctuation, plus the math, currency, and
/// modifier symbols. Reading the category rather than testing the ASCII range
/// is what puts an em dash, guillemets, and an ideographic full stop in the same
/// class as their ASCII counterparts, so a run of mixed punctuation is one `w`
/// stop instead of one per script. Every ASCII punctuation character falls in
/// this set, so ASCII text classes exactly as a range test leaves it.
///
/// The symbol categories are here because a `+` or a `$` reads as punctuation
/// between words even though neither is punctuation in the typographic sense.
fn char_is_punctuation(ch: char) -> bool {
    matches!(
        get_general_category(ch),
        GeneralCategory::OtherPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::MathSymbol
            | GeneralCategory::CurrencySymbol
            | GeneralCategory::ModifierSymbol
    )
}

fn char_is_line_ending(ch: char) -> bool {
    matches!(ch, '\n' | '\r')
}

/// Whether `ch` belongs to a word for the short-word motions and the word
/// completer, which is any alphanumeric plus the underscore.
pub fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Category for the long-word (`W`/`B`) motions, where everything that is not
/// whitespace is one class.
///
/// This is vim's WORD, so `W` runs straight through a symbol like `foo→bar` and
/// treats it as a single word. Helix instead breaks at symbols outside the word
/// and punctuation classes, so the two disagree on such text. The vim reading is
/// deliberate. The point of `W` is to ignore the fine-grained classes `w` uses,
/// and a third class that only some symbols fall into makes the motion harder to
/// predict than either rule alone.
fn long_word_category(ch: char) -> CharCategory {
    if char_is_line_ending(ch) {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else {
        CharCategory::Word
    }
}

/// The scan position for a forward motion whose block cursor sits on the char
/// at `from`, one cell past it. Converts [`next_word_end`]'s cursor-cell contract
/// into the scan-position contract the `*_range` fns expect.
fn forward_scan_start(rope: &Rope, from: usize) -> usize {
    from + rope.chars_at(from).next().map_or(0, |c| c.len_utf8())
}

/// A Helix `range_to_target` step for the next-word-start motion. Given the
/// origin `(anchor, head)` -- where `head` is the scan position, one cell past
/// the block-cursor cell -- returns the new `(anchor, head)`. Threading feeds the
/// returned head back in as the next origin, and the anchor advances past a
/// leading newline run and onto each new span start, so a counted motion selects
/// only the final word span rather than accumulating every word it crosses.
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

/// End of the next word after the block-cursor cell at `from`.
pub fn next_word_end(rope: &Rope, from: usize) -> usize {
    let from = forward_scan_start(rope, from);
    next_word_end_with(rope, from, from, categorize_char).1
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

/// Shared forward word-motion scan matching Helix's `range_to_target`.
///
/// `from` is the scan position -- one past the block-cursor cell -- and
/// `prev_ch` is seeded from the char just before it, so the boundary straddling
/// the cursor is visible to the scan. That seeding is what makes count threading
/// work. Each iteration passes the previous head back in as `from`, so the
/// `head == head_start -> anchor = head` rule advances the anchor onto every new
/// span start rather than only across newline crossings.
///
/// Returns the target `head` and an `anchor` that starts at `anchor_in`,
/// advances past a leading newline run (so the head runs through a blank line),
/// and advances onto the first target boundary at `head_start` (so a motion from
/// whitespace does not select the gap). A `None` `prev_ch` (scan starting at the
/// buffer front) counts as an unconditional target, mirroring Helix.
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
    let mut anchor = anchor_in;
    let mut head = from;
    let mut prev_ch = rope.reversed_chars_at(from).next();

    let mut chars = rope.chars_at(from).peekable();
    while let Some(&ch) = chars.peek() {
        if !char_is_line_ending(ch) {
            break;
        }
        chars.next();
        head += ch.len_utf8();
        prev_ch = Some(ch);
    }
    if prev_ch.is_some_and(char_is_line_ending) {
        anchor = head;
    }

    let head_start = head;
    loop {
        let Some(ch) = chars.next() else {
            return (anchor, head);
        };
        let reached = match prev_ch {
            None => true,
            Some(prev) => is_target(prev, ch, category(prev) != category(ch)),
        };
        if reached {
            if head == head_start {
                anchor = head;
            } else {
                return (anchor, head);
            }
        }
        prev_ch = Some(ch);
        head += ch.len_utf8();
    }
}

pub fn prev_word_start(rope: &Rope, from: usize) -> usize {
    prev_word_start_with(rope, from, from, categorize_char).1
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
/// [`forward_word_range`]. Here `from` is the block-cursor cell and `prev_ch` is
/// seeded from the char at `from`, so the scan proceeds toward the buffer start.
///
/// Returns the target `head` and an `anchor` that starts at `anchor_in`, retreats
/// past a trailing newline run (so the head runs back through a blank line), and
/// retreats onto the head when the cell at `from` is itself a line ending (so
/// `b` on a newline excludes it) or at the first target boundary at `head_start`.
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

    while let Some(&ch) = iter.peek() {
        if !char_is_line_ending(ch) {
            break;
        }
        iter.next();
        head -= ch.len_utf8();
        prev_ch = ch;
    }
    if char_is_line_ending(prev_ch) {
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
/// [`compute_number_delta`]). Trailing `_`s are excluded from the
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

/// Returns `text` with `delta` added to the number it holds, written back in
/// the format it arrived in, or [`None`] when it does not parse as `kind`.
///
/// The counterpart to [`find_number_at`]: that returns a literal's range and
/// category, this rewrites the literal in place. Incrementing a number in a
/// source file must not reformat it, so the shape of the original survives the
/// arithmetic. A leading zero marks a decimal as a fixed-width field and the
/// width is kept, widening by one where a sign appears. A radix literal keeps
/// its marker, its body's hex case, its zero padding, and its underscore
/// grouping, which is re-laid from the right so an overflow into a new group
/// stays even.
///
/// Values saturate rather than wrap at the ends of [`i64`] for a decimal and
/// [`u64`] for a radix literal.
pub fn compute_number_delta(text: &str, kind: NumberKind, delta: i64) -> Option<String> {
    match kind {
        NumberKind::Decimal => {
            let parsed = text.parse::<i64>().ok()?;
            let new_value = parsed.saturating_add(delta);

            // A leading zero marks the literal as a fixed-width field, so the
            // width is carried over rather than letting the number shrink out
            // of it. The sign takes one of those columns, which is why crossing
            // zero moves the width by one.
            if !text.starts_with('0') && !text.starts_with("-0") {
                return Some(new_value.to_string());
            }
            let width = match (parsed.is_negative(), new_value.is_negative()) {
                (true, false) => text.len() - 1,
                (false, true) => text.len() + 1,
                _ => text.len(),
            };

            // The `0` flag rather than a `0>` fill, which would pad ahead of
            // the sign and give `00-7` instead of `-007`.
            Some(format!("{new_value:0width$}"))
        },
        _ => {
            let mut chars = text.chars();
            chars.next()?;
            let marker = chars.next()?;
            let body = &text[2..];

            let digits_only: String = body.chars().filter(|c| *c != '_').collect();
            if digits_only.is_empty() {
                return None;
            }

            let parsed = u64::from_str_radix(&digits_only, kind.radix()).ok()?;
            let new_value = if delta < 0 {
                parsed.saturating_sub(delta.unsigned_abs())
            } else {
                parsed.saturating_add(delta as u64)
            };

            let body_uppercase = matches!(kind, NumberKind::Hex)
                && (marker.is_ascii_uppercase()
                    || body
                        .chars()
                        .any(|c| c.is_ascii_uppercase() && c.is_ascii_alphabetic()));
            let new_body = match (kind, body_uppercase) {
                (NumberKind::Hex, true) => format!("{new_value:X}"),
                (NumberKind::Hex, false) => format!("{new_value:x}"),
                (NumberKind::Binary, _) => format!("{new_value:b}"),
                (NumberKind::Octal, _) => format!("{new_value:o}"),
                _ => unreachable!(),
            };

            let padded = if new_body.len() < digits_only.len() {
                format!("{new_body:0>width$}", width = digits_only.len())
            } else {
                new_body
            };

            let formatted = match group_size_for_body(body) {
                Some(g) => regroup_right(&padded, g),
                None => padded,
            };

            Some(format!("0{marker}{formatted}"))
        },
    }
}

fn group_size_for_body(body: &str) -> Option<usize> {
    let trimmed = body.trim_matches('_');
    let last = trimmed.rfind('_')?;
    Some(trimmed.len() - last - 1)
}

fn regroup_right(digits: &str, group_size: usize) -> String {
    let n = digits.len();
    if n == 0 || group_size == 0 || n <= group_size {
        return digits.to_string();
    }
    let first_size = if n.is_multiple_of(group_size) {
        group_size
    } else {
        n % group_size
    };
    let mut out = String::with_capacity(n + (n - 1) / group_size);
    out.push_str(&digits[..first_size]);
    let mut idx = first_size;
    while idx < n {
        out.push('_');
        out.push_str(&digits[idx..idx + group_size]);
        idx += group_size;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    /// Drive a forward `*_range` scan from the block-cursor cell at `from`,
    /// returning the resulting head. Mirrors how the removed singular wrappers
    /// advanced `from` past its char before scanning.
    fn fwd_head(r: &Rope, from: usize, f: impl Fn(&Rope, usize, usize) -> (usize, usize)) -> usize {
        let s = forward_scan_start(r, from);
        f(r, s, s).1
    }

    /// Drive a backward `*_range` scan from the block-cursor cell at `from`,
    /// returning the resulting head. Backward scans take the cursor cell
    /// directly, so no advance is needed.
    fn bwd_head(r: &Rope, from: usize, f: impl Fn(&Rope, usize, usize) -> (usize, usize)) -> usize {
        f(r, from, from).1
    }

    #[test]
    fn next_word_start_range_advances_anchor_like_helix() {
        // Mirrors Helix's move_next_word_start, called as the handler does with
        // the origin `(anchor, head)` where head is the block edge (one past the
        // cursor cell). Each string is short enough to read every offset directly.
        let cases: &[(&str, usize, usize, (usize, usize))] = &[
            // On a word start, anchor stays and head reaches the next word.
            ("ab cd", 0, 1, (0, 3)),
            // Mid-word, the anchor stays at its origin.
            ("hello", 2, 3, (2, 5)),
            // One leading space (a single boundary char) advances the anchor onto
            // the word start, and the head runs through the first word to the next.
            (" ab cd", 0, 1, (1, 4)),
            // A multi-space run leaves the anchor put. Head stops at the word start.
            ("  ab cd", 0, 1, (0, 2)),
            // A leading newline run skips the anchor past it. Head runs the word.
            ("\n\nab cd", 0, 1, (2, 5)),
            // A longer whitespace run behaves like the two-space one. Its target
            // is only reached after the head has moved, so the anchor stays.
            ("   ab cd", 0, 1, (0, 3)),
            // Newlines and spaces alternating. The anchor follows the first
            // newline run, and the head stops at the next newline rather than
            // running through the space between them.
            (" \n \nab", 0, 1, (2, 3)),
            // Starting inside a punctuation run. The boundary onto the word is
            // reached while the head is still at its start, so the anchor moves
            // onto the word rather than keeping the rest of the run.
            ("ab..cd ef", 3, 4, (4, 7)),
        ];
        for (text, anchor, head, expected) in cases {
            assert_eq!(
                next_word_start_range(&rope(text), *anchor, *head),
                *expected,
                "next_word_start on {text:?} from ({anchor}, {head})"
            );
        }
    }

    #[test]
    fn next_word_start_range_threads_anchor_across_counts() {
        // Feeding each result back in as the next origin advances the anchor onto
        // every new span start, so a counted `w` selects only the final span.
        let r = rope("abc def ghi");
        let first = next_word_start_range(&r, 0, 1);
        assert_eq!(first, (0, 4));
        let second = next_word_start_range(&r, first.0, first.1);
        assert_eq!(second, (4, 8));
    }

    /// A counted motion that runs out of buffer settles rather than drifting.
    ///
    /// The handler threads each result back in as the next origin and stops when
    /// a step returns its input unchanged, so a count larger than the words
    /// remaining depends on the scan reaching a fixed point rather than, say,
    /// walking the anchor forward each time.
    #[test]
    fn next_word_start_range_settles_when_the_count_overshoots() {
        let r = rope("ab cd");
        let first = next_word_start_range(&r, 0, 1);
        assert_eq!(first, (0, 3));
        let second = next_word_start_range(&r, first.0, first.1);
        assert_eq!(
            second,
            (3, 5),
            "the last word, with the anchor on its start"
        );
        assert_eq!(
            next_word_start_range(&r, second.0, second.1),
            second,
            "a further step is a no-op, which is what ends a counted motion",
        );
    }

    #[test]
    fn prev_word_start_range_excludes_a_cursor_newline() {
        // `b` with the block cursor on the newline retreats the anchor onto the
        // head so the newline is excluded, matching Helix.
        assert_eq!(prev_word_start_range(&rope("abc def\nghi"), 8, 7), (7, 4));
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
            // From inside whitespace the anchor stays at the block edge, since
            // the boundary out of the run is not a backward target.
            ("ab   cd", 4, 3, (4, 0)),
            // From a word start with punctuation behind it, the anchor retreats
            // onto the seed and the head reaches the run's start.
            ("ab..cd", 5, 4, (4, 2)),
            // Newlines and spaces alternating, mirroring the forward case. The
            // head stops at the space run rather than crossing to the newlines.
            ("ab\n\n  cd", 7, 6, (6, 4)),
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
        for ch in ['!', '"', '#', '(', ')', '-', '/', '@', '[', '\\', '{', '~'] {
            assert_eq!(
                categorize_char(ch),
                CharCategory::Punctuation,
                "ASCII punctuation classes as it always did: {ch}",
            );
        }
        assert_eq!(
            categorize_char('\u{2014}'),
            CharCategory::Punctuation,
            "an em dash carries a punctuation category, so it joins the class",
        );
        assert_eq!(
            categorize_char('\u{3002}'),
            CharCategory::Punctuation,
            "and so does an ideographic full stop",
        );
        assert_eq!(
            categorize_char('\u{00AB}'),
            CharCategory::Punctuation,
            "and a guillemet, which is initial punctuation",
        );
        assert_eq!(
            categorize_char('+'),
            CharCategory::Punctuation,
            "a math symbol counts, reading as punctuation between words",
        );
        assert_eq!(
            categorize_char('\u{00A3}'),
            CharCategory::Punctuation,
            "as does a currency symbol",
        );
        assert_eq!(
            categorize_char('\u{2192}'),
            CharCategory::Punctuation,
            "an arrow is a math symbol, so it joins them",
        );
        assert_eq!(
            categorize_char('\u{00A9}'),
            CharCategory::Unknown,
            "a copyright sign is an other-symbol, outside the ten categories",
        );
    }

    /// A run of mixed punctuation is one `w` stop, not one per script.
    ///
    /// Classing by ASCII range put an em dash in its own class, so `w` broke
    /// between it and the full stop beside it. Reading the Unicode category puts
    /// both in `Punctuation`, and the run travels together.
    #[test]
    fn a_word_motion_takes_mixed_punctuation_as_one_span() {
        let rope = Rope::from("a\u{2014}.b");
        assert_eq!(
            next_word_start_range(&rope, 0, 1),
            (1, 5),
            "the em dash and the full stop are one span",
        );
    }

    /// A combining mark categorizes apart from the letter it sits on, so a
    /// motion counting codepoints stops between the two.
    ///
    /// These pin what the motion returns, which is an offset inside a
    /// character. That is deliberate. The invariant that a selection covers
    /// whole characters is applied where selections are written, so every
    /// producer is covered at once rather than each learning the rule. See
    /// `SelectionsCollection::replace_with`.
    #[test]
    fn a_combining_mark_is_its_own_category() {
        assert_eq!(categorize_char('e'), CharCategory::Word);
        assert_eq!(categorize_char('\u{301}'), CharCategory::Unknown);
    }

    #[test]
    fn next_word_end_stops_inside_a_decomposed_cluster() {
        // "cafe" + combining acute at 4..6, so the cluster runs 3..6.
        let r = rope("cafe\u{301} bar");
        assert_eq!(fwd_head(&r, 0, next_word_end_range), 4);
    }

    #[test]
    fn next_word_start_stops_inside_a_decomposed_cluster() {
        let r = rope("cafe\u{301} bar");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 4);
    }

    #[test]
    fn a_precomposed_accent_is_one_word_character() {
        // The same word spelled with U+00E9 has no separate mark, so the motion
        // runs to the space. This is what makes the decomposed cases above a
        // property of the spelling rather than of the letter.
        let r = rope("caf\u{e9} bar");
        assert_eq!(fwd_head(&r, 0, next_word_end_range), 5);
    }

    #[test]
    fn next_word_start_basic() {
        let r = rope("hello world");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 6);
        assert_eq!(fwd_head(&r, 6, next_word_start_range), 11);
    }

    #[test]
    fn next_word_start_from_whitespace_jumps_across_word() {
        let r = rope("hello world foo");
        assert_eq!(fwd_head(&r, 5, next_word_start_range), 12);
    }

    #[test]
    fn next_word_start_three_words() {
        let r = rope("abc def ghi");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 4);
        assert_eq!(fwd_head(&r, 4, next_word_start_range), 8);
        assert_eq!(fwd_head(&r, 8, next_word_start_range), 11);
    }

    #[test]
    fn next_word_start_at_end_is_noop() {
        let r = rope("hello");
        assert_eq!(fwd_head(&r, 5, next_word_start_range), 5);
    }

    /// A forward scan starting on a trailing newline yields an empty range at
    /// the buffer end.
    ///
    /// The anchor tracks the newline the scan began after, and with nothing left
    /// to scan the head never leaves it, so both ends land on the length. That is
    /// this layer's answer and not a defect: widening an empty landing to the
    /// one-cell block cursor belongs to whoever writes the selection, since only
    /// there is it known whether a cursor or a range is being produced.
    #[test]
    fn next_word_start_on_a_trailing_newline_yields_an_empty_range() {
        let r = rope("foo\n");
        let start = forward_scan_start(&r, 3);
        assert_eq!(
            next_word_start_range(&r, start, start),
            (4, 4),
            "nothing left to scan, so the range collapses onto the end",
        );
        assert_eq!(
            next_word_end_range(&r, start, start),
            (4, 4),
            "and the word-end variant agrees",
        );
    }

    #[test]
    fn next_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 0);
    }

    #[test]
    fn next_word_start_underscore_is_word() {
        let r = rope("foo_bar baz");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 8);
    }

    #[test]
    fn next_word_start_punctuation_boundary() {
        let r = rope("alphanumeric.and");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 12);
        assert_eq!(fwd_head(&r, 12, next_word_start_range), 16);
    }

    #[test]
    fn next_word_start_punctuation_group_boundary() {
        let r = rope("alphanumeric.!,and");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 12);
        assert_eq!(fwd_head(&r, 12, next_word_start_range), 15);
    }

    #[test]
    fn next_word_start_stops_on_newline() {
        let r = rope("foo\nbar");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 3);
        assert_eq!(fwd_head(&r, 3, next_word_start_range), 7);
    }

    #[test]
    fn next_word_start_bridges_consecutive_newlines() {
        let r = rope("foo\n\nbar");
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 3);
        // From inside the blank run the head bridges both newlines and runs
        // through the following word to its end, matching Helix.
        assert_eq!(fwd_head(&r, 3, next_word_start_range), 8);
    }

    #[test]
    fn next_word_start_multibyte() {
        let r = rope("héllo wörld");
        let world_start = "héllo ".len();
        assert_eq!(fwd_head(&r, 0, next_word_start_range), world_start);
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
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 8);
    }

    #[test]
    fn prev_word_end_basic() {
        let r = rope("hello world");
        assert_eq!(bwd_head(&r, 9, prev_word_end_range), 5);
    }

    #[test]
    fn prev_word_end_from_end() {
        let r = rope("hello world");
        assert_eq!(bwd_head(&r, 11, prev_word_end_range), 5);
    }

    #[test]
    fn prev_word_end_from_word_start() {
        let r = rope("foo bar");
        assert_eq!(bwd_head(&r, 4, prev_word_end_range), 3);
    }

    #[test]
    fn prev_word_end_from_whitespace_skips_prev_word() {
        let r = rope("foo bar baz");
        assert_eq!(bwd_head(&r, 7, prev_word_end_range), 3);
    }

    #[test]
    fn prev_word_end_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(bwd_head(&r, 0, prev_word_end_range), 0);
    }

    #[test]
    fn prev_word_end_empty_rope() {
        let r = rope("");
        assert_eq!(bwd_head(&r, 0, prev_word_end_range), 0);
    }

    #[test]
    fn prev_word_end_punctuation() {
        let r = rope("abc.def");
        assert_eq!(bwd_head(&r, 7, prev_word_end_range), 4);
        assert_eq!(bwd_head(&r, 4, prev_word_end_range), 3);
    }

    #[test]
    fn prev_word_end_multibyte() {
        let r = rope("héllo wörld");
        let world_end = r.len();
        let hello_end = "héllo".len();
        assert_eq!(bwd_head(&r, world_end, prev_word_end_range), hello_end);
    }

    #[test]
    fn prev_word_end_all_newlines() {
        let r = rope("\n\n\n\n\n");
        assert_eq!(bwd_head(&r, 5, prev_word_end_range), 0);
    }

    #[test]
    fn next_long_word_start_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        assert_eq!(fwd_head(&r, 0, next_long_word_start_range), 8);
        assert_eq!(fwd_head(&r, 0, next_word_start_range), 3);
    }

    #[test]
    fn next_long_word_start_crosses_a_symbol() {
        // "foo→bar" is one long word, because everything non-whitespace is one
        // class. Helix breaks at the arrow, so this pins the divergence rather
        // than leaving it to be read as an oversight. The arrow is three bytes,
        // putting the next word start at 10.
        let r = rope("foo\u{2192}bar baz");
        assert_eq!(fwd_head(&r, 0, next_long_word_start_range), 10);
        assert_eq!(
            fwd_head(&r, 0, next_word_start_range),
            3,
            "the short-word motion does stop at the arrow",
        );
    }

    #[test]
    fn next_long_word_start_chained_punctuation() {
        let r = rope("a!@b cd ef");
        assert_eq!(fwd_head(&r, 0, next_long_word_start_range), 5);
        assert_eq!(fwd_head(&r, 5, next_long_word_start_range), 8);
    }

    #[test]
    fn next_long_word_start_stops_on_newline() {
        let r = rope("foo.bar\nbaz");
        assert_eq!(fwd_head(&r, 0, next_long_word_start_range), 7);
        assert_eq!(fwd_head(&r, 7, next_long_word_start_range), 11);
    }

    #[test]
    fn next_long_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(fwd_head(&r, 0, next_long_word_start_range), 0);
    }

    #[test]
    fn next_long_word_end_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        assert_eq!(fwd_head(&r, 0, next_long_word_end_range), 7);
        assert_eq!(next_word_end(&r, 0), 3);
    }

    #[test]
    fn next_long_word_end_chained_punctuation() {
        let r = rope("a!@b cd");
        assert_eq!(fwd_head(&r, 0, next_long_word_end_range), 4);
    }

    #[test]
    fn next_long_word_end_multibyte() {
        let r = rope("foo.héllo wörld");
        let hello_end = "foo.héllo".len();
        assert_eq!(fwd_head(&r, 0, next_long_word_end_range), hello_end);
    }

    #[test]
    fn prev_long_word_start_treats_punctuation_as_word() {
        let r = rope("foo bar.baz");
        assert_eq!(bwd_head(&r, 11, prev_long_word_start_range), 4);
        assert_eq!(prev_word_start(&r, 11), 8);
    }

    #[test]
    fn prev_long_word_start_chained_punctuation() {
        let r = rope("ab cd ef!@g");
        let len = r.len();
        assert_eq!(bwd_head(&r, len, prev_long_word_start_range), 6);
    }

    #[test]
    fn prev_long_word_start_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(bwd_head(&r, 0, prev_long_word_start_range), 0);
    }

    #[test]
    fn prev_long_word_end_treats_punctuation_as_word() {
        let r = rope("foo.bar baz");
        let len = r.len();
        assert_eq!(bwd_head(&r, len, prev_long_word_end_range), 7);
        assert_eq!(bwd_head(&r, len, prev_word_end_range), 7);
    }

    #[test]
    fn prev_long_word_end_skips_internal_punctuation_boundary() {
        let r = rope("aa bb.cc dd");
        assert_eq!(bwd_head(&r, 6, prev_long_word_end_range), 2);
        assert_eq!(bwd_head(&r, 6, prev_word_end_range), 5);
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

    /// A decimal written with a leading zero is a fixed-width field, so it
    /// keeps that width instead of collapsing to the shortest form.
    ///
    /// Crossing zero moves the width by one, because the sign occupies a column
    /// of its own. A literal without a leading zero is left to size itself.
    #[test]
    fn incrementing_a_zero_padded_decimal_keeps_its_width() {
        for (text, delta, want) in [
            ("007", 1, "008"),
            ("-08", 1, "-07"),
            ("-01", 1, "00"),
            ("01", -2, "-01"),
            ("09", 1, "10"),
            ("7", 1, "8"),
        ] {
            assert_eq!(
                compute_number_delta(text, NumberKind::Decimal, delta).as_deref(),
                Some(want),
                "{text} incremented by {delta}",
            );
        }
    }

    /// A radix literal is rewritten in the case and width it arrived in.
    ///
    /// The case is taken from the whole literal, marker included, so an
    /// uppercase `0X` uppercases the digits even over a lowercase body.
    /// Digits added by the increment take that case too, and a body that
    /// shrinks is zero-padded back to the width it held rather than losing a
    /// column.
    #[test]
    fn incrementing_a_radix_literal_keeps_its_case_and_width() {
        for (text, kind, delta, want) in [
            ("0xff", NumberKind::Hex, 1, "0x100"),
            ("0xFF", NumberKind::Hex, 1, "0x100"),
            ("0XfF", NumberKind::Hex, 1, "0X100"),
            ("0Xfe", NumberKind::Hex, 1, "0XFF"),
            ("0xfe", NumberKind::Hex, 1, "0xff"),
            ("0x0f", NumberKind::Hex, 1, "0x10"),
            ("0x10", NumberKind::Hex, -1, "0x0f"),
            ("0x00ff", NumberKind::Hex, 1, "0x0100"),
            ("0b0111", NumberKind::Binary, 1, "0b1000"),
            ("0o077", NumberKind::Octal, 1, "0o100"),
        ] {
            assert_eq!(
                compute_number_delta(text, kind, delta).as_deref(),
                Some(want),
                "{text} incremented by {delta}",
            );
        }
    }

    /// An underscored body keeps its grouping, re-laid from the right.
    ///
    /// The group size comes from the last separator alone, so `ff_ff` is read
    /// as groups of two and stays in twos, rather than as one group of four.
    /// Laying the groups out from the right is what keeps an
    /// increment that overflows into a new group even, leaving the short group
    /// at the front where a reader expects it.
    #[test]
    fn incrementing_an_underscored_literal_regroups_it() {
        for (text, kind, delta, want) in [
            ("0xff_ff", NumberKind::Hex, 1, "0x1_00_00"),
            ("0xff_fe", NumberKind::Hex, 1, "0xff_ff"),
            ("0b1111_1111", NumberKind::Binary, 1, "0b1_0000_0000"),
            ("0xf_ff", NumberKind::Hex, 1, "0x10_00"),
        ] {
            assert_eq!(
                compute_number_delta(text, kind, delta).as_deref(),
                Some(want),
                "{text} incremented by {delta}",
            );
        }
    }

    /// A body that is only separators has no digits to parse, and a decimal
    /// that is not one is left alone rather than rewritten as zero.
    #[test]
    fn a_literal_that_does_not_parse_yields_nothing() {
        assert_eq!(compute_number_delta("0x__", NumberKind::Hex, 1), None);
        assert_eq!(compute_number_delta("beef", NumberKind::Decimal, 1), None);
        assert_eq!(compute_number_delta("", NumberKind::Hex, 1), None);
    }
}
