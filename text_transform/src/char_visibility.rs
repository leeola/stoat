//! Character visibility detection for text editors.
//!
//! This module provides utilities to detect and handle invisible Unicode characters
//! that might be confusing in code editing contexts. It identifies characters that
//! have no visual representation but affect text layout or semantics.
//!
//! # Categories of Invisible Characters
//!
//! - ASCII control codes (0x00-0x1F, 0x7F) except whitespace
//! - C1 control codes (0x80-0x9F)
//! - Unicode format characters (Cf category)
//! - Zero-width characters (ZWS, ZWNJ, ZWJ, etc.)
//! - Variation selectors
//! - Special blank characters (Braille blank, Hangul fillers)

/// Determines if a character is invisible and should be highlighted in editors.
///
/// Invisible characters are those that have no visual representation but may affect
/// text behavior. This includes control codes, format characters, and zero-width chars.
///
/// Notable exceptions:
/// - Tab, newline, and carriage return are visible whitespace
/// - Ideographic space (U+3000) is visually wide, not highlighted
/// - Space (U+0020) is visible whitespace
pub fn is_invisible(c: char) -> bool {
    let codepoint = c as u32;

    // ASCII control codes except visible whitespace
    if codepoint <= 0x1F {
        return c != '\t' && c != '\n' && c != '\r';
    }

    // DEL and C1 control codes
    if (0x7F..=0x9F).contains(&codepoint) {
        return true;
    }

    // Unicode whitespace (except regular space and ideographic space)
    if c.is_whitespace() && c != ' ' && c != '\u{3000}' {
        return true;
    }

    // Unicode format characters
    if in_range(c, FORMAT_CHARS) {
        return true;
    }

    // Other known invisible characters
    if in_range(c, INVISIBLE_CHARS) {
        return true;
    }

    false
}

/// Returns a visible replacement glyph for an invisible character.
///
/// Returns `None` if the character should be preserved (e.g., combining characters),
/// `Some(glyph)` with a visible replacement otherwise.
///
/// Replacement strategy:
/// - ASCII controls get Unicode control picture symbols
/// - DEL gets its control picture
/// - Preserved characters (used in combining) return None
/// - Other invisibles get a fixed-width space
pub fn replacement_glyph(c: char) -> Option<&'static str> {
    let codepoint = c as u32;

    // ASCII control codes have specific symbols
    if codepoint <= 0x1F {
        return Some(CONTROL_PICTURES[codepoint as usize]);
    }

    // DEL has its own symbol
    if c == '\u{7F}' {
        return Some(DEL_SYMBOL);
    }

    // Don't replace characters used in combining/joining
    if in_range(c, PRESERVED_CHARS) {
        return None;
    }

    // Everything else gets a visible space
    Some(REPLACEMENT_SPACE)
}

/// Fixed-width space used for replacing most invisible characters.
const REPLACEMENT_SPACE: &str = "\u{2007}";

/// Symbol for DEL character.
const DEL_SYMBOL: &str = "\u{2421}";

/// Unicode control picture symbols for ASCII control codes (U+2400-U+241F).
/// Index corresponds to the control code value (0x00-0x1F).
const CONTROL_PICTURES: &[&str] = &[
    "\u{2400}", "\u{2401}", "\u{2402}", "\u{2403}", "\u{2404}", "\u{2405}", "\u{2406}", "\u{2407}",
    "\u{2408}", "\u{2409}", "\u{240A}", "\u{240B}", "\u{240C}", "\u{240D}", "\u{240E}", "\u{240F}",
    "\u{2410}", "\u{2411}", "\u{2412}", "\u{2413}", "\u{2414}", "\u{2415}", "\u{2416}", "\u{2417}",
    "\u{2418}", "\u{2419}", "\u{241A}", "\u{241B}", "\u{241C}", "\u{241D}", "\u{241E}", "\u{241F}",
];

/// Unicode Format category (Cf) characters.
/// Generated from Unicode 16.0.0 database using category Cf.
/// These are formatting characters like BOM, RTL marks, etc.
const FORMAT_CHARS: &[(char, char)] = &[
    ('\u{AD}', '\u{AD}'),       // Soft hyphen
    ('\u{600}', '\u{605}'),     // Arabic format chars
    ('\u{61C}', '\u{61C}'),     // Arabic letter mark
    ('\u{6DD}', '\u{6DD}'),     // Arabic end of ayah
    ('\u{70F}', '\u{70F}'),     // Syriac abbreviation mark
    ('\u{890}', '\u{891}'),     // Arabic pound/piastre marks
    ('\u{8E2}', '\u{8E2}'),     // Arabic disputed end of ayah
    ('\u{180E}', '\u{180E}'),   // Mongolian vowel separator
    ('\u{200B}', '\u{200F}'),   // Zero-width space, joiners, marks
    ('\u{202A}', '\u{202E}'),   // Directional formatting
    ('\u{2060}', '\u{2064}'),   // Word joiner, invisible operators
    ('\u{2066}', '\u{206F}'),   // Directional isolates
    ('\u{FEFF}', '\u{FEFF}'),   // Zero-width no-break space (BOM)
    ('\u{FFF9}', '\u{FFFB}'),   // Interlinear annotation
    ('\u{110BD}', '\u{110BD}'), // Kaithi number sign
    ('\u{110CD}', '\u{110CD}'), // Kaithi number sign above
    ('\u{13430}', '\u{1343F}'), // Egyptian hieroglyph format controls
    ('\u{1BCA0}', '\u{1BCA3}'), // Shorthand format controls
    ('\u{1D173}', '\u{1D17A}'), // Musical formatting
    ('\u{E0001}', '\u{E0001}'), // Language tag
    ('\u{E0020}', '\u{E007F}'), // Tag characters
];

/// Other invisible characters not in Unicode Cf category.
/// Includes variation selectors, special blanks, and zero-width chars.
const INVISIBLE_CHARS: &[(char, char)] = &[
    ('\u{034F}', '\u{034F}'),   // Combining grapheme joiner
    ('\u{115F}', '\u{1160}'),   // Hangul choseong fillers
    ('\u{17B4}', '\u{17B5}'),   // Khmer vowel inherent
    ('\u{180B}', '\u{180D}'),   // Mongolian free variation selectors
    ('\u{2800}', '\u{2800}'),   // Braille pattern blank
    ('\u{3164}', '\u{3164}'),   // Hangul filler
    ('\u{FE00}', '\u{FE0F}'),   // Variation selectors
    ('\u{FFA0}', '\u{FFA0}'),   // Halfwidth Hangul filler
    ('\u{FFFC}', '\u{FFFC}'),   // Object replacement character
    ('\u{E0100}', '\u{E01EF}'), // Variation selectors supplement
];

/// Characters that should be preserved (not replaced) because they're used
/// in combining sequences, emoji joining, or complex scripts.
const PRESERVED_CHARS: &[(char, char)] = &[
    ('\u{034F}', '\u{034F}'), // Combining grapheme joiner
    ('\u{200D}', '\u{200D}'), // Zero-width joiner (emoji, complex scripts)
    ('\u{17B4}', '\u{17B5}'), // Khmer vowel inherent
    ('\u{180B}', '\u{180D}'), // Mongolian FVS (used in Mongolian script)
];

/// Checks if a character falls within any of the given ranges.
/// Ranges are expected to be sorted and non-overlapping.
fn in_range(c: char, ranges: &[(char, char)]) -> bool {
    for &(start, end) in ranges {
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
    use super::*;

    #[test]
    fn ascii_controls_are_invisible() {
        assert!(is_invisible('\x00')); // NUL
        assert!(is_invisible('\x01')); // SOH
        assert!(is_invisible('\x1F')); // Unit separator
    }

    #[test]
    fn visible_whitespace_not_invisible() {
        assert!(!is_invisible('\t'));
        assert!(!is_invisible('\n'));
        assert!(!is_invisible('\r'));
        assert!(!is_invisible(' '));
    }

    #[test]
    fn del_and_c1_controls() {
        assert!(is_invisible('\u{7F}')); // DEL
        assert!(is_invisible('\u{80}')); // First C1
        assert!(is_invisible('\u{9F}')); // Last C1
    }

    #[test]
    fn unicode_whitespace() {
        assert!(is_invisible('\u{00A0}')); // Non-breaking space
        assert!(is_invisible('\u{200B}')); // Zero-width space
        assert!(!is_invisible('\u{3000}')); // Ideographic space (wide)
    }

    #[test]
    fn format_characters() {
        assert!(is_invisible('\u{AD}')); // Soft hyphen
        assert!(is_invisible('\u{FEFF}')); // BOM
        assert!(is_invisible('\u{202E}')); // Right-to-left override
    }

    #[test]
    fn special_invisible_chars() {
        assert!(is_invisible('\u{FE00}')); // Variation selector
        assert!(is_invisible('\u{2800}')); // Braille blank
        assert!(is_invisible('\u{115F}')); // Hangul filler
    }

    #[test]
    fn regular_chars_not_invisible() {
        assert!(!is_invisible('a'));
        assert!(!is_invisible('0'));
        assert!(!is_invisible('.'));
    }

    #[test]
    fn control_picture_replacements() {
        assert_eq!(replacement_glyph('\x00'), Some("\u{2400}"));
        assert_eq!(replacement_glyph('\x1F'), Some("\u{241F}"));
        assert_eq!(replacement_glyph('\u{7F}'), Some("\u{2421}"));
    }

    #[test]
    fn preserved_chars_not_replaced() {
        assert_eq!(replacement_glyph('\u{200D}'), None); // ZWJ
        assert_eq!(replacement_glyph('\u{034F}'), None); // CGJ
    }

    #[test]
    fn other_invisibles_get_space() {
        assert_eq!(replacement_glyph('\u{AD}'), Some("\u{2007}"));
        assert_eq!(replacement_glyph('\u{FEFF}'), Some("\u{2007}"));
        assert_eq!(replacement_glyph('\u{80}'), Some("\u{2007}"));
    }

    #[test]
    fn in_range_helper() {
        let ranges = &[('\u{100}', '\u{1FF}'), ('\u{300}', '\u{3FF}')];
        assert!(in_range('\u{150}', ranges));
        assert!(in_range('\u{350}', ranges));
        assert!(!in_range('\u{50}', ranges));
        assert!(!in_range('\u{250}', ranges));
    }
}
