//! Input simulation for testing and debugging.
//!
//! Provides parsing and simulation of keystroke sequences from string input.
//! Supports a simple DSL where regular characters are typed normally and special
//! keys are enclosed in angle brackets like `<Esc>`, `<Enter>`, etc.

use gpui::Keystroke;

/// Parse an input sequence string into individual keystrokes.
///
/// The input DSL supports:
/// - Regular characters: typed as-is (e.g., "hello" becomes 5 keystrokes)
/// - Special keys: enclosed in angle brackets (e.g., "<Esc>", "<Enter>", "<Tab>")
/// - Case-sensitive: "a" and "A" are different keystrokes
///
/// # Examples
///
/// ```rust,ignore
/// // Type "Hello" then escape
/// parse_input_sequence("Hello<Esc>")
///
/// // Enter insert mode, type text, exit, save
/// parse_input_sequence("iHello World<Esc>:w<Enter>")
///
/// // Navigate directory in command line
/// parse_input_sequence(":cd foo<Enter>")
/// ```
pub fn parse_input_sequence(input: &str) -> Vec<Keystroke> {
    let mut keystrokes = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            // Parse special key sequence
            let mut key_name = String::new();
            let mut found_close = false;

            while let Some(&next_ch) = chars.peek() {
                chars.next();
                if next_ch == '>' {
                    found_close = true;
                    break;
                }
                key_name.push(next_ch);
            }

            if found_close && !key_name.is_empty() {
                // Try to parse as special key
                match parse_special_key(&key_name) {
                    Some(keystroke) => keystrokes.push(keystroke),
                    None => {
                        // Not a recognized special key, treat as literal
                        tracing::warn!("Unknown special key: <{}>", key_name);
                        // Push the characters literally
                        keystrokes.push(Keystroke::parse("<").unwrap());
                        for c in key_name.chars() {
                            keystrokes.push(Keystroke::parse(&c.to_string()).unwrap());
                        }
                        keystrokes.push(Keystroke::parse(">").unwrap());
                    },
                }
            } else {
                // Unclosed angle bracket, treat as literal
                keystrokes.push(Keystroke::parse("<").unwrap());
            }
        } else {
            // Regular character
            keystrokes.push(Keystroke::parse(&ch.to_string()).unwrap());
        }
    }

    keystrokes
}

/// Parse a special key name into a Keystroke.
///
/// Recognizes common special keys and returns None for unrecognized names.
fn parse_special_key(name: &str) -> Option<Keystroke> {
    // Map common special key names to their keystroke representations
    let key = match name.to_lowercase().as_str() {
        "esc" | "escape" => "escape",
        "enter" | "return" | "cr" => "enter",
        "tab" => "tab",
        "backspace" | "bs" => "backspace",
        "delete" | "del" => "delete",
        "space" | "spc" => "space",
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        "pageup" | "pgup" => "pageup",
        "pagedown" | "pgdn" => "pagedown",
        "home" => "home",
        "end" => "end",
        _ => return None,
    };

    Keystroke::parse(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_text() {
        let keystrokes = parse_input_sequence("hello");
        assert_eq!(keystrokes.len(), 5);
        assert_eq!(keystrokes[0].key, "h");
        assert_eq!(keystrokes[4].key, "o");
    }

    #[test]
    fn with_escape() {
        let keystrokes = parse_input_sequence("test<Esc>");
        assert_eq!(keystrokes.len(), 5);
        assert_eq!(keystrokes[4].key, "escape");
    }

    #[test]
    fn with_enter() {
        let keystrokes = parse_input_sequence(":w<Enter>");
        assert_eq!(keystrokes.len(), 3);
        assert_eq!(keystrokes[0].key, ":");
        assert_eq!(keystrokes[1].key, "w");
        assert_eq!(keystrokes[2].key, "enter");
    }

    #[test]
    fn insert_mode_sequence() {
        let keystrokes = parse_input_sequence("iHello<Esc>");
        assert_eq!(keystrokes.len(), 7);
        assert_eq!(keystrokes[0].key, "i");
        assert_eq!(keystrokes[6].key, "escape");
    }

    #[test]
    fn command_line_sequence() {
        let keystrokes = parse_input_sequence(":cd foo<Enter>");
        assert_eq!(keystrokes.len(), 8);
        assert_eq!(keystrokes[0].key, ":");
        assert_eq!(keystrokes[7].key, "enter");
    }

    #[test]
    fn unknown_special_key() {
        let keystrokes = parse_input_sequence("<Unknown>");
        // Should treat as literal: '<', 'U', 'n', 'k', 'n', 'o', 'w', 'n', '>'
        assert_eq!(keystrokes.len(), 9);
        assert_eq!(keystrokes[0].key, "<");
        assert_eq!(keystrokes[8].key, ">");
    }

    #[test]
    fn unclosed_angle_bracket() {
        let keystrokes = parse_input_sequence("test<");
        assert_eq!(keystrokes.len(), 5);
        assert_eq!(keystrokes[4].key, "<");
    }

    #[test]
    fn multiple_special_keys() {
        let keystrokes = parse_input_sequence("<Tab><Enter><Esc>");
        assert_eq!(keystrokes.len(), 3);
        assert_eq!(keystrokes[0].key, "tab");
        assert_eq!(keystrokes[1].key, "enter");
        assert_eq!(keystrokes[2].key, "escape");
    }

    #[test]
    fn mixed_text_and_special() {
        let keystrokes = parse_input_sequence("hello<Space>world<Enter>");
        assert_eq!(keystrokes.len(), 12);
        assert_eq!(keystrokes[5].key, "space");
        assert_eq!(keystrokes[11].key, "enter");
    }
}
