use super::key::{Key, ModifiedKey, NamedKey};

/// Parse Vim-like key notation into Keys
/// Examples:
///   "abc" -> [Char('a'), Char('b'), Char('c')]
///   "c?" -> [Char('c'), Modified(Shift('/'))]  // ? is Shift+/
///   "c<S-/>" -> [Char('c'), Modified(Shift('/'))]  // explicit
///   "<Esc>" -> [Named(Esc)]
pub fn parse_keys(notation: &str) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    let mut chars = notation.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                // Parse special key <...>
                let mut special = String::new();
                let mut found_closing = false;

                while let Some(ch) = chars.next() {
                    if ch == '>' {
                        found_closing = true;
                        break;
                    }
                    special.push(ch);
                }

                if !found_closing {
                    return Err(format!("Unclosed special key notation at '<{}'", special));
                }

                keys.push(parse_special(&special)?);
            },
            '?' => {
                // Convenience: ? means Shift+/
                keys.push(Key::Modified(ModifiedKey::Shift('/')));
            },
            _ => {
                keys.push(Key::Char(ch));
            },
        }
    }

    Ok(keys)
}

fn parse_special(s: &str) -> Result<Key, String> {
    // Handle modifiers
    if let Some(dash) = s.find('-') {
        let (mod_str, key_str) = s.split_at(dash);
        let key_str = &key_str[1..]; // Skip the dash

        if key_str.len() != 1 {
            return Err(format!("Invalid modified key: <{}>", s));
        }

        let key_char = key_str.chars().next().unwrap();

        match mod_str {
            "C" | "Ctrl" => Ok(Key::Modified(ModifiedKey::Ctrl(key_char))),
            "S" | "Shift" => Ok(Key::Modified(ModifiedKey::Shift(key_char))),
            "A" | "Alt" | "M" | "Meta" => Ok(Key::Modified(ModifiedKey::Alt(key_char))),
            _ => Err(format!("Unknown modifier: {}", mod_str)),
        }
    } else {
        // Named keys
        match s {
            "Esc" | "Escape" => Ok(Key::Named(NamedKey::Esc)),
            "Enter" | "Return" | "CR" => Ok(Key::Named(NamedKey::Enter)),
            "Tab" => Ok(Key::Named(NamedKey::Tab)),
            "Space" => Ok(Key::Named(NamedKey::Space)),
            "Backspace" | "BS" => Ok(Key::Named(NamedKey::Backspace)),
            "Delete" | "Del" => Ok(Key::Named(NamedKey::Delete)),
            "Up" => Ok(Key::Named(NamedKey::Up)),
            "Down" => Ok(Key::Named(NamedKey::Down)),
            "Left" => Ok(Key::Named(NamedKey::Left)),
            "Right" => Ok(Key::Named(NamedKey::Right)),
            "Home" => Ok(Key::Named(NamedKey::Home)),
            "End" => Ok(Key::Named(NamedKey::End)),
            "PageUp" | "PgUp" => Ok(Key::Named(NamedKey::PageUp)),
            "PageDown" | "PgDn" => Ok(Key::Named(NamedKey::PageDown)),
            _ => Err(format!("Unknown special key: <{}>", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_keys() {
        let keys = parse_keys("abc").unwrap();
        assert_eq!(keys, vec![Key::Char('a'), Key::Char('b'), Key::Char('c'),]);
    }

    #[test]
    fn test_parse_question_mark_convenience() {
        let keys = parse_keys("c?").unwrap();
        assert_eq!(
            keys,
            vec![Key::Char('c'), Key::Modified(ModifiedKey::Shift('/')),]
        );
    }

    #[test]
    fn test_parse_special_keys() {
        let keys = parse_keys("a<Esc>b<Enter>").unwrap();
        assert_eq!(
            keys,
            vec![
                Key::Char('a'),
                Key::Named(NamedKey::Esc),
                Key::Char('b'),
                Key::Named(NamedKey::Enter),
            ]
        );
    }

    #[test]
    fn test_parse_modified_keys() {
        let keys = parse_keys("<C-a><S-/><A-x>").unwrap();
        assert_eq!(
            keys,
            vec![
                Key::Modified(ModifiedKey::Ctrl('a')),
                Key::Modified(ModifiedKey::Shift('/')),
                Key::Modified(ModifiedKey::Alt('x')),
            ]
        );
    }

    #[test]
    fn test_canvas_help_sequence() {
        let keys = parse_keys("c<S-/>").unwrap();
        assert_eq!(
            keys,
            vec![Key::Char('c'), Key::Modified(ModifiedKey::Shift('/')),]
        );

        // Both notations should be equivalent
        let keys2 = parse_keys("c?").unwrap();
        assert_eq!(keys, keys2);
    }

    #[test]
    fn test_complex_sequence() {
        let keys = parse_keys("i<Esc>c?a<Esc>").unwrap();
        assert_eq!(
            keys,
            vec![
                Key::Char('i'),
                Key::Named(NamedKey::Esc),
                Key::Char('c'),
                Key::Modified(ModifiedKey::Shift('/')),
                Key::Char('a'),
                Key::Named(NamedKey::Esc),
            ]
        );
    }

    #[test]
    fn test_error_handling() {
        // Unclosed bracket
        assert!(parse_keys("a<Esc").is_err());

        // Unknown modifier
        assert!(parse_keys("<X-a>").is_err());

        // Unknown special key
        assert!(parse_keys("<Unknown>").is_err());

        // Invalid modified key format
        assert!(parse_keys("<C-abc>").is_err());
    }
}
