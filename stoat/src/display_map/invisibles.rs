//! Classification of Unicode characters that render blank or near-blank and
//! are easily confused with ordinary spacing -- ASCII and C1 control codes,
//! non-ASCII whitespace, and the Format / invisible Nonspacing-Mark categories
//! (byte-order marks, bidirectional overrides, zero-width spaces, variation
//! selectors). Marking them guards against homoglyph and bidi-override source
//! spoofing and surfaces stray formatting characters.
//!
//! Unassigned codepoints are deliberately left unmarked: the font renderer
//! already substitutes a replacement glyph for those, and there are a great
//! many of them.

/// Whether `c` should be surfaced as a stray invisible. ASCII tab, newline,
/// and carriage return are excluded (they have structural meaning); the
/// ideographic space is excluded because it already renders visibly wide.
pub fn is_invisible(c: char) -> bool {
    if c <= '\u{1f}' {
        c != '\t' && c != '\n' && c != '\r'
    } else if c >= '\u{7f}' {
        c <= '\u{9f}'
            || (c.is_whitespace() && c != IDEOGRAPHIC_SPACE)
            || contains(c, FORMAT)
            || contains(c, OTHER)
    } else {
        false
    }
}

/// The single visible cell to substitute for a marked invisible, or `None`
/// for the [`PRESERVE`] set -- combining characters (e.g. ZWJ) that sit inside
/// a composed glyph and must be kept verbatim so emoji and similar are not
/// shredded.
///
/// Only meaningful for characters [`is_invisible`] accepts; a visible
/// character passed here returns a fixed-width space.
pub fn replacement(c: char) -> Option<char> {
    if c <= '\u{1f}' {
        // C0 control pictures occupy U+2400..=U+241F, one per control code.
        char::from_u32(0x2400 + c as u32)
    } else if c == '\u{7f}' {
        Some('\u{2421}')
    } else if contains(c, PRESERVE) {
        None
    } else {
        Some('\u{2007}')
    }
}

/// Common alongside wide character sets and already rendered visibly wide, so
/// it is not treated as a stray invisible.
const IDEOGRAPHIC_SPACE: char = '\u{3000}';

/// General-category Format characters (Unicode 16).
const FORMAT: &[(char, char)] = &[
    ('\u{ad}', '\u{ad}'),
    ('\u{600}', '\u{605}'),
    ('\u{61c}', '\u{61c}'),
    ('\u{6dd}', '\u{6dd}'),
    ('\u{70f}', '\u{70f}'),
    ('\u{890}', '\u{891}'),
    ('\u{8e2}', '\u{8e2}'),
    ('\u{180e}', '\u{180e}'),
    ('\u{200b}', '\u{200f}'),
    ('\u{202a}', '\u{202e}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2066}', '\u{206f}'),
    ('\u{feff}', '\u{feff}'),
    ('\u{fff9}', '\u{fffb}'),
    ('\u{110bd}', '\u{110bd}'),
    ('\u{110cd}', '\u{110cd}'),
    ('\u{13430}', '\u{1343f}'),
    ('\u{1bca0}', '\u{1bca3}'),
    ('\u{1d173}', '\u{1d17a}'),
    ('\u{e0001}', '\u{e0001}'),
    ('\u{e0020}', '\u{e007f}'),
];

/// Other blank or invisible Nonspacing Marks (excluding Format). Variation
/// selectors stop at FE0D: VS15/VS16 (FE0E/FE0F) select text vs. emoji
/// presentation and must not be marked.
const OTHER: &[(char, char)] = &[
    ('\u{034f}', '\u{034f}'),
    ('\u{115f}', '\u{1160}'),
    ('\u{17b4}', '\u{17b5}'),
    ('\u{180b}', '\u{180d}'),
    ('\u{2800}', '\u{2800}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{fe00}', '\u{fe0d}'),
    ('\u{ffa0}', '\u{ffa0}'),
    ('\u{fffc}', '\u{fffc}'),
    ('\u{e0100}', '\u{e01ef}'),
];

/// The subset of [`FORMAT`]/[`OTHER`] that appears within composed glyphs;
/// [`replacement`] keeps these verbatim rather than substituting a cell.
const PRESERVE: &[(char, char)] = &[
    ('\u{034f}', '\u{034f}'),
    ('\u{200d}', '\u{200d}'),
    ('\u{17b4}', '\u{17b5}'),
    ('\u{180b}', '\u{180d}'),
    ('\u{e0061}', '\u{e007a}'),
    ('\u{e007f}', '\u{e007f}'),
];

/// Membership test over a start-sorted list of inclusive ranges.
fn contains(c: char, list: &[(char, char)]) -> bool {
    for &(start, end) in list {
        if c < start {
            return false;
        }
        if c <= end {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{is_invisible, replacement};

    #[test]
    fn controls_and_c1_are_invisible() {
        assert!(is_invisible('\u{0000}'));
        assert!(is_invisible('\u{001f}'));
        assert!(is_invisible('\u{007f}'));
        assert!(is_invisible('\u{0085}'));
        assert!(!is_invisible('\t'));
        assert!(!is_invisible('\n'));
        assert!(!is_invisible('\r'));
    }

    #[test]
    fn ordinary_chars_not_invisible() {
        assert!(!is_invisible('a'));
        assert!(!is_invisible(' '));
        assert!(!is_invisible('世'));
        assert!(!is_invisible('\u{3000}'));
    }

    #[test]
    fn spoofing_chars_marked() {
        assert!(is_invisible('\u{202e}'));
        assert!(is_invisible('\u{00a0}'));
        assert!(is_invisible('\u{200b}'));
        assert_eq!(replacement('\u{202e}'), Some('\u{2007}'));
        assert_eq!(replacement('\u{00a0}'), Some('\u{2007}'));
    }

    #[test]
    fn combining_chars_invisible_but_preserved() {
        assert!(is_invisible('\u{200d}'));
        assert_eq!(replacement('\u{200d}'), None);
        assert_eq!(replacement('\u{034f}'), None);
    }

    #[test]
    fn variation_selectors_15_16_not_marked() {
        assert!(!is_invisible('\u{fe0e}'));
        assert!(!is_invisible('\u{fe0f}'));
    }

    #[test]
    fn control_pictures() {
        assert_eq!(replacement('\u{0000}'), Some('\u{2400}'));
        assert_eq!(replacement('\u{001f}'), Some('\u{241f}'));
        assert_eq!(replacement('\u{007f}'), Some('\u{2421}'));
    }
}
