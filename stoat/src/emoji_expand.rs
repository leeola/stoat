//! Swapping a typed `:name:` for the emoji it names.
//!
//! The closing colon finishes what the shortcode completion server offers, so
//! a reader who types `:smile:` straight through never has to touch the popup.
//! [`hook_insert`] answers what that colon does, the way
//! [`stoat_text::auto_pairs::hook_insert`] answers what a typed bracket does.
//!
//! This lives here rather than beside that hook because it needs the emoji
//! table, and the text crate has no business carrying one.
//!
//! The boundary rule is defined here and read by
//! [`crate::lsp::emoji`], so the swap and the completion agree on when a colon
//! opens a shortcode rather than each keeping its own copy.

use stoat_text::Rope;

/// How far back a shortcode is looked for.
///
/// The longest gemoji name is well inside this. The bound is what keeps a line
/// of colon-separated text from being walked end to end on every typed colon.
const MAX_SHORTCODE_BYTES: usize = 64;

/// A `:name:` the typed colon closes, and what it becomes.
pub(crate) struct Expansion {
    /// Offset of the opening colon. The span from here to the cursor is what
    /// the emoji replaces, the typed colon included by never being written.
    pub(crate) open: usize,
    pub(crate) emoji: &'static str,
}

/// What typing `:` at `cursor` expands to, or `None` to type the colon as
/// written.
///
/// Everything that only looks like a shortcode answers `None`. That is a name
/// no emoji answers to, an empty one, a colon that does not open at a word
/// boundary (which leaves `std::` and a `x: T` annotation alone), and a name
/// longer than any real shortcode.
pub(crate) fn hook_insert(rope: &Rope, cursor: usize) -> Option<Expansion> {
    let mut chars = rope.reversed_chars_at(cursor);
    let mut open = cursor;

    let found_open = loop {
        let Some(ch) = chars.next() else {
            break false;
        };
        open -= ch.len_utf8();
        if cursor - open > MAX_SHORTCODE_BYTES {
            break false;
        }
        if ch == ':' {
            break true;
        }
        if !is_shortcode_char(ch) {
            break false;
        }
    };
    if !found_open || open + 1 == cursor {
        return None;
    }
    if !opens_a_shortcode(chars.next()) {
        return None;
    }

    let name = rope.slice(open + 1..cursor).to_string();
    let emoji = emojis::get_by_shortcode(&name)?;
    Some(Expansion {
        open,
        emoji: emoji.as_str(),
    })
}

/// Whether `ch` appears in a gemoji shortcode.
pub(crate) fn is_shortcode_char(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '+' | '-')
}

/// Whether a colon preceded by `before` opens a shortcode.
///
/// `None` is the start of the text or line, which opens one. A `:` counts as a
/// word character so the second colon of a `std::` path never opens one.
pub(crate) fn opens_a_shortcode(before: Option<char>) -> bool {
    before.is_none_or(|ch| !(ch.is_alphanumeric() || matches!(ch, '_' | ':')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(text: &str, cursor: usize) -> Option<(usize, &'static str)> {
        hook_insert(&Rope::from(text), cursor).map(|e| (e.open, e.emoji))
    }

    #[test]
    fn a_closing_colon_swaps_the_name_it_closes() {
        assert_eq!(expand(":smile", 6), Some((0, "\u{1f604}")));
        assert_eq!(expand("hi :smile", 9), Some((3, "\u{1f604}")));
        assert_eq!(
            expand("(:smile", 7),
            Some((1, "\u{1f604}")),
            "a paren is a boundary like a space",
        );
    }

    /// Every case here holds a real shortcode name, so only the boundary rule
    /// rejects it. A name no emoji answers to fails the lookup either way.
    #[test]
    fn a_colon_that_does_not_open_at_a_boundary_expands_nothing() {
        assert_eq!(expand("x:smile", 7), None, "the colon follows a letter");
        assert_eq!(expand("std::smile", 10), None, "and sits inside a path");
        assert_eq!(expand("1:smile", 7), None, "and follows a digit");
        assert_eq!(expand("a_:smile", 8), None, "and follows an underscore");
    }

    #[test]
    fn code_that_only_looks_like_a_shortcode_is_typed_as_written() {
        assert_eq!(expand("std:", 4), None, "the second colon of a path");
        assert_eq!(expand(":", 1), None, "an empty name");
        assert_eq!(expand(":notaname", 9), None, "a name no emoji answers to");
        assert_eq!(expand("smile", 5), None, "no opening colon at all");
    }

    #[test]
    fn a_name_longer_than_any_shortcode_is_not_searched_for() {
        let long = format!(":{}", "a".repeat(MAX_SHORTCODE_BYTES));
        let cursor = long.len();
        assert_eq!(expand(&long, cursor), None);
    }

    #[test]
    fn a_multi_codepoint_emoji_expands_whole() {
        let (_, emoji) = expand(":+1", 3).expect("+1 is a gemoji shortcode");
        assert_eq!(emoji, "\u{1f44d}");

        let (_, family) = expand(":family_man_woman_boy", 21).expect("a ZWJ sequence");
        assert!(
            family.chars().count() > 1,
            "the whole sequence comes through: {family:?}",
        );
    }

    #[test]
    fn only_text_before_the_cursor_is_read() {
        assert_eq!(
            expand(":smile:extra", 6),
            Some((0, "\u{1f604}")),
            "what follows the cursor is not part of the name",
        );
    }
}
