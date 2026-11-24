pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{0000}'..='\u{0008}'
        | '\u{000E}'..='\u{001F}'
        | '\u{007F}'
        | '\u{0080}'..='\u{009F}'
        | '\u{00AD}'
        | '\u{034F}'
        | '\u{061C}'
        | '\u{115F}'..='\u{1160}'
        | '\u{17B4}'..='\u{17B5}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{0600}'..='\u{0605}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}'
        | '\u{E0100}'..='\u{E01EF}'
    )
}

pub fn replacement(c: char) -> Option<&'static str> {
    match c {
        '\u{0000}' => Some("NUL"),
        '\u{0001}' => Some("SOH"),
        '\u{0002}' => Some("STX"),
        '\u{0003}' => Some("ETX"),
        '\u{0004}' => Some("EOT"),
        '\u{0005}' => Some("ENQ"),
        '\u{0006}' => Some("ACK"),
        '\u{0007}' => Some("BEL"),
        '\u{0008}' => Some("BS"),
        '\u{000E}' => Some("SO"),
        '\u{000F}' => Some("SI"),
        '\u{007F}' => Some("DEL"),
        '\u{00AD}' => Some("SHY"),
        '\u{200B}' => Some("ZWSP"),
        '\u{200C}' => Some("ZWNJ"),
        '\u{200D}' => Some("ZWJ"),
        '\u{200E}' => Some("LRM"),
        '\u{200F}' => Some("RLM"),
        '\u{FEFF}' => Some("BOM"),
        _ if is_invisible(c) => Some("?"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_invisible, replacement};

    #[test]
    fn control_chars_detected() {
        assert!(is_invisible('\u{0000}'));
        assert!(is_invisible('\u{0001}'));
        assert!(is_invisible('\u{007F}'));
        assert!(is_invisible('\u{200B}'));
        assert!(is_invisible('\u{FEFF}'));
    }

    #[test]
    fn normal_chars_not_detected() {
        assert!(!is_invisible('a'));
        assert!(!is_invisible('Z'));
        assert!(!is_invisible(' '));
        assert!(!is_invisible('\t'));
        assert!(!is_invisible('\n'));
    }

    #[test]
    fn replacements_correct() {
        assert_eq!(replacement('\u{0000}'), Some("NUL"));
        assert_eq!(replacement('\u{007F}'), Some("DEL"));
        assert_eq!(replacement('\u{200B}'), Some("ZWSP"));
        assert_eq!(replacement('\u{FEFF}'), Some("BOM"));
        assert_eq!(replacement('a'), None);
    }
}
