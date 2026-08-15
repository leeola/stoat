use crate::Rope;
use unicode_general_category::{get_general_category, GeneralCategory};

/// Digit grouping mark a number literal carries, which
/// [`integer_increment`] preserves rather than normalizes away.
const SEPARATOR: char = '_';

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

/// Whether the short-word (`w`/`b`/`e`) motions break between `a` and `b`,
/// which is wherever their classes differ.
fn is_word_boundary(a: char, b: char) -> bool {
    categorize_char(a) != categorize_char(b)
}

/// Whether the long-word (`W`/`B`) motions break between `a` and `b`.
///
/// Every class change except the one between a word and punctuation, so `W`
/// runs through `foo.bar` and `foo→bar` as one word while still stopping at
/// whitespace and at a line ending.
///
/// What it does not run through is `Unknown`, the class nothing else claimed. A
/// copyright sign or an emoji breaks a long word where a full stop does not,
/// since the point of `W` is to ignore the punctuation `w` stops at rather than
/// to ignore every distinction the text makes.
fn is_long_word_boundary(a: char, b: char) -> bool {
    match (categorize_char(a), categorize_char(b)) {
        (CharCategory::Word, CharCategory::Punctuation)
        | (CharCategory::Punctuation, CharCategory::Word) => false,
        (a, b) => a != b,
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
    next_word_start_with(rope, anchor, head, is_word_boundary)
}

pub fn next_long_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_start_with(rope, anchor, head, is_long_word_boundary)
}

fn next_word_start_with<F: Fn(char, char) -> bool>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    is_boundary: F,
) -> (usize, usize) {
    forward_word_range(
        rope,
        anchor_in,
        from,
        &is_boundary,
        |_prev, ch, boundary| boundary && (char_is_line_ending(ch) || !ch.is_whitespace()),
    )
}

/// End of the next word after the block-cursor cell at `from`.
pub fn next_word_end(rope: &Rope, from: usize) -> usize {
    let from = forward_scan_start(rope, from);
    next_word_end_with(rope, from, from, is_word_boundary).1
}

pub fn next_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_end_with(rope, anchor, head, is_word_boundary)
}

pub fn next_long_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    next_word_end_with(rope, anchor, head, is_long_word_boundary)
}

fn next_word_end_with<F: Fn(char, char) -> bool>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    is_boundary: F,
) -> (usize, usize) {
    forward_word_range(rope, anchor_in, from, &is_boundary, |prev, ch, boundary| {
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
    is_boundary: C,
    is_target: T,
) -> (usize, usize)
where
    C: Fn(char, char) -> bool,
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
            Some(prev) => is_target(prev, ch, is_boundary(prev, ch)),
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
    prev_word_start_with(rope, from, from, is_word_boundary).1
}

/// [`prev_word_start`] as a range_to_target step: given the origin
/// `(anchor, head)`, returns the new `(anchor, head)`. The anchor retreats past
/// a trailing newline run and past a single trailing boundary char, so a
/// backward word motion from whitespace or after a boundary does not keep the
/// gap in the selection.
pub fn prev_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_start_with(rope, anchor, head, is_word_boundary)
}

pub fn prev_long_word_start_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_start_with(rope, anchor, head, is_long_word_boundary)
}

fn prev_word_start_with<F: Fn(char, char) -> bool>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    is_boundary: F,
) -> (usize, usize) {
    backward_word_range(rope, anchor_in, from, &is_boundary, |prev, ch, boundary| {
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
    is_boundary: C,
    is_target: T,
) -> (usize, usize)
where
    C: Fn(char, char) -> bool,
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
        let boundary = is_boundary(prev_ch, ch);
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
    prev_word_end_with(rope, anchor, head, is_word_boundary)
}

pub fn prev_long_word_end_range(rope: &Rope, anchor: usize, head: usize) -> (usize, usize) {
    prev_word_end_with(rope, anchor, head, is_long_word_boundary)
}

fn prev_word_end_with<F: Fn(char, char) -> bool>(
    rope: &Rope,
    anchor_in: usize,
    from: usize,
    is_boundary: F,
) -> (usize, usize) {
    backward_word_range(
        rope,
        anchor_in,
        from,
        &is_boundary,
        |_prev, ch, boundary| boundary && (!ch.is_whitespace() || char_is_line_ending(ch)),
    )
}

/// Returns `text` with `amount` added to the integer it spells, or [`None`]
/// when it spells no integer.
///
/// The whole of `text` must be the number. A caller hands over the text a
/// selection covers, so a selection holding anything else answers `None` and
/// the caller leaves it alone.
///
/// Recognizes decimal with no prefix, and base 2, 8, and 16 behind `0b`, `0o`,
/// and `0x`. A decimal goes negative where a radix literal saturates at zero,
/// since a radix literal has no sign to write. Both ends saturate rather than
/// wrap.
///
/// Incrementing a number in a source file must not reformat it, so the written
/// form survives the arithmetic. A leading zero marks a fixed-width field and
/// the width is kept, widening by one where a sign appears. Hex digits follow
/// whichever case the original used more of. `_` separators are re-laid at the
/// distances they arrived at, and a number that outgrows its width gains a
/// separator at the same spacing.
///
/// A leading or trailing `_` answers `None`, since neither belongs to a number.
pub fn integer_increment(text: &str, amount: i64) -> Option<String> {
    if text.is_empty() || text.starts_with(SEPARATOR) || text.ends_with(SEPARATOR) {
        return None;
    }

    let radix = match &text[..text.len().min(2)] {
        "0x" => 16,
        "0o" => 8,
        "0b" => 2,
        _ => 10,
    };

    // Right-to-left so the offsets survive a number that grows or shrinks a
    // digit, which is what lets the separators go back where they were.
    let separator_rtl_indexes: Vec<usize> = text
        .chars()
        .rev()
        .enumerate()
        .filter_map(|(i, c)| (c == SEPARATOR).then_some(i))
        .collect();

    let word: String = text.chars().filter(|&c| c != SEPARATOR).collect();

    let mut new_text = if radix == 10 {
        let value = i128::from_str_radix(&word, radix).ok()?;
        let new_value = value.saturating_add(amount as i128);

        let format_length = match (value.is_negative(), new_value.is_negative()) {
            (true, false) => word.len() - 1,
            (false, true) => word.len() + 1,
            _ => word.len(),
        } - separator_rtl_indexes.len();

        if word.starts_with('0') || word.starts_with("-0") {
            format!("{new_value:0format_length$}")
        } else {
            format!("{new_value}")
        }
    } else {
        let body = &word[2..];
        let value = u128::from_str_radix(body, radix).ok()?;
        let new_value = (value as i128).saturating_add(amount as i128).max(0);
        let format_length = text.len() - 2 - separator_rtl_indexes.len();

        match radix {
            2 => format!("0b{new_value:0format_length$b}"),
            8 => format!("0o{new_value:0format_length$o}"),
            _ => {
                let (lower, upper) = body.chars().fold((0usize, 0usize), |(lo, up), c| {
                    (
                        lo + c.is_ascii_lowercase() as usize,
                        up + c.is_ascii_uppercase() as usize,
                    )
                });
                if upper > lower {
                    format!("0x{new_value:0format_length$X}")
                } else {
                    format!("0x{new_value:0format_length$x}")
                }
            },
        }
    };

    for &rtl_index in &separator_rtl_indexes {
        if rtl_index < new_text.len() {
            let new_index = new_text.len().saturating_sub(rtl_index);
            if new_index > 0 {
                new_text.insert(new_index, SEPARATOR);
            }
        }
    }

    // A number that outgrew its width has room the old offsets never covered,
    // so keep laying separators leftward at the spacing they already hold.
    if new_text.len() > text.len() && !separator_rtl_indexes.is_empty() {
        let spacing = match separator_rtl_indexes.as_slice() {
            [.., b, a] => a - b - 1,
            _ => separator_rtl_indexes[0],
        };

        let prefix_length = if radix == 10 { 0 } else { 2 };
        if let Some(mut index) = new_text.find(SEPARATOR) {
            while index - prefix_length > spacing {
                index -= spacing;
                new_text.insert(index, SEPARATOR);
            }
        }
    }

    Some(new_text)
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

    /// `W` ignores the word/punctuation split `w` stops at, and nothing else.
    #[test]
    fn a_long_word_runs_through_punctuation_but_not_through_unknown() {
        assert!(!is_long_word_boundary('a', '.'), "foo.bar is one long word");
        assert!(!is_long_word_boundary('.', 'a'), "and so is .foo");
        assert!(
            !is_long_word_boundary('a', '\u{2192}'),
            "an arrow is a math symbol, so it counts as punctuation here",
        );
        assert!(!is_long_word_boundary('a', 'b'), "nor does a word break");
        assert!(!is_long_word_boundary('.', '!'), "nor does punctuation");

        assert!(
            is_long_word_boundary('a', ' '),
            "whitespace still breaks it"
        );
        assert!(
            is_long_word_boundary('a', '\n'),
            "and so does a line ending"
        );
        assert!(
            is_long_word_boundary('a', '\u{00A9}'),
            "a copyright sign is Unknown, which W does not run through",
        );
        assert!(
            is_long_word_boundary('.', '\u{00A9}'),
            "the exception is word-to-punctuation alone, not any pair",
        );
    }

    /// The divergence from vim's WORD, which folds every non-whitespace
    /// character into one class and so runs straight past the sign.
    #[test]
    fn a_long_word_start_stops_at_an_other_symbol() {
        let rope = Rope::from("foo\u{00A9}bar baz");
        assert_eq!(
            next_long_word_start_range(&rope, 0, 1).1,
            3,
            "the motion lands on the copyright sign, not the space after bar",
        );
    }

    fn incremented(cases: &[(&str, i64, &str)]) {
        for &(original, amount, expected) in cases {
            assert_eq!(
                integer_increment(original, amount).as_deref(),
                Some(expected),
                "{original} by {amount}",
            );
        }
    }

    #[test]
    fn integer_increment_walks_decimals_across_zero() {
        incremented(&[
            ("100", 1, "101"),
            ("100", -1, "99"),
            ("99", 1, "100"),
            ("100", 1000, "1100"),
            ("100", -1000, "-900"),
            ("-1", 1, "0"),
            ("-1", 2, "1"),
            ("1", -1, "0"),
            ("1", -2, "-1"),
        ]);
    }

    #[test]
    fn integer_increment_keeps_hexadecimal_case_and_width() {
        incremented(&[
            ("0x0100", 1, "0x0101"),
            ("0x0100", -1, "0x00ff"),
            ("0x0001", -1, "0x0000"),
            ("0x0000", -1, "0x0000"),
            ("0xffffffffffffffff", 1, "0x10000000000000000"),
            ("0xffffffffffffffff", 2, "0x10000000000000001"),
            ("0xffffffffffffffff", -1, "0xfffffffffffffffe"),
            ("0xABCDEF1234567890", 1, "0xABCDEF1234567891"),
            ("0xabcdef1234567890", 1, "0xabcdef1234567891"),
        ]);
    }

    #[test]
    fn integer_increment_carries_octal_literals() {
        incremented(&[
            ("0o0107", 1, "0o0110"),
            ("0o0110", -1, "0o0107"),
            ("0o0001", -1, "0o0000"),
            ("0o7777", 1, "0o10000"),
            ("0o1000", -1, "0o0777"),
            ("0o0107", 10, "0o0121"),
            ("0o0000", -1, "0o0000"),
            ("0o1777777777777777777777", 1, "0o2000000000000000000000"),
            ("0o1777777777777777777777", 2, "0o2000000000000000000001"),
            ("0o1777777777777777777777", -1, "0o1777777777777777777776"),
        ]);
    }

    #[test]
    fn integer_increment_carries_binary_literals() {
        incremented(&[
            ("0b00000100", 1, "0b00000101"),
            ("0b00000100", -1, "0b00000011"),
            ("0b00000100", 2, "0b00000110"),
            ("0b00000100", -2, "0b00000010"),
            ("0b00000001", -1, "0b00000000"),
            ("0b00111111", 10, "0b01001001"),
            ("0b11111111", 1, "0b100000000"),
            ("0b10000000", -1, "0b01111111"),
            ("0b0000", -1, "0b0000"),
            (
                "0b1111111111111111111111111111111111111111111111111111111111111111",
                1,
                "0b10000000000000000000000000000000000000000000000000000000000000000",
            ),
            (
                "0b1111111111111111111111111111111111111111111111111111111111111111",
                2,
                "0b10000000000000000000000000000000000000000000000000000000000000001",
            ),
            (
                "0b1111111111111111111111111111111111111111111111111111111111111111",
                -1,
                "0b1111111111111111111111111111111111111111111111111111111111111110",
            ),
        ]);
    }

    #[test]
    fn integer_increment_relays_the_separators() {
        incremented(&[
            ("999_999", 1, "1_000_000"),
            ("1_000_000", -1, "999_999"),
            ("-999_999", -1, "-1_000_000"),
            ("0x0000_0000_0001", 0x1_ffff_0000, "0x0001_ffff_0001"),
            ("0x0000_0000", -1, "0x0000_0000"),
            ("0x0000_0000_0000", -1, "0x0000_0000_0000"),
            ("0b01111111_11111111", 1, "0b10000000_00000000"),
            ("0b11111111_11111111", 1, "0b1_00000000_00000000"),
        ]);
    }

    #[test]
    fn integer_increment_rejects_an_edge_separator() {
        assert_eq!(integer_increment("9_", 1), None);
        assert_eq!(integer_increment("_9", 1), None);
        assert_eq!(integer_increment("_9_", 1), None);
    }
}
