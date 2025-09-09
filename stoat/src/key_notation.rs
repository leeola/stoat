//! Key notation parsing for vim-like key sequences.
//!
//! This module provides functionality to parse string representations of
//! keyboard input into [`EditorEvent`]s. It supports vim-like notation
//! including special keys in angle brackets and modifier combinations.

use crate::{
    events::EditorEvent,
    input::{Modifiers, keys},
};

/// Parses a keyboard input string into a sequence of [`EditorEvent`]s.
///
/// This function converts vim-like keyboard notation into actual key events.
/// Special keys are enclosed in angle brackets (e.g., `<Esc>`, `<Enter>`).
///
/// # Examples
///
/// ```rust
/// use stoat::key_notation::parse_sequence;
///
/// let events = parse_sequence("iHello<Esc>");
/// assert_eq!(events.len(), 7); // i, H, e, l, l, o, Esc
///
/// let events = parse_sequence("<C-a>");
/// assert_eq!(events.len(), 1); // Ctrl+A
/// ```
pub fn parse_sequence(input: &str) -> Vec<EditorEvent> {
    let mut events = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            // Parse special key sequence
            let mut key_seq = String::new();
            let mut found_closing = false;

            while let Some(&next_ch) = chars.peek() {
                if next_ch == '>' {
                    chars.next(); // consume '>'
                    found_closing = true;
                    break;
                }
                key_seq.push(
                    chars
                        .next()
                        .expect("chars should have next since we peeked"),
                );
            }

            if !found_closing {
                // Treat as literal '<' if no closing '>'
                events.push(create_char_event(ch));
                // Also push the characters we collected as literals
                for ch in key_seq.chars() {
                    events.push(create_char_event(ch));
                }
                continue;
            }

            // Parse the key sequence
            if let Some(event) = parse_special_key(&key_seq) {
                events.push(event);
            }
        } else {
            // Regular character
            events.push(create_char_event(ch));
        }
    }

    events
}

/// Creates a key press event for a regular character.
fn create_char_event(ch: char) -> EditorEvent {
    // Handle space and tab as named keys
    let key = match ch {
        ' ' => keys::SPACE.to_string(),
        '\t' => keys::TAB.to_string(),
        _ => ch.to_string(),
    };

    EditorEvent::KeyPress {
        key,
        modifiers: Modifiers::default(),
    }
}

/// Parses a special key sequence (without angle brackets) into an event.
///
/// Handles both unmodified special keys (Esc, Enter, etc.) and
/// modified keys (C-a, S-Tab, A-x, etc.).
fn parse_special_key(seq: &str) -> Option<EditorEvent> {
    // Check for modifier keys first
    if let Some(event) = parse_modified_key(seq) {
        return Some(event);
    }

    // Parse unmodified special keys
    let key = match seq.to_lowercase().as_str() {
        "esc" | "escape" => keys::ESCAPE.to_string(),
        "enter" | "return" | "cr" => keys::ENTER.to_string(),
        "tab" => keys::TAB.to_string(),
        "bs" | "backspace" => keys::BACKSPACE.to_string(),
        "del" | "delete" => keys::DELETE.to_string(),
        "space" => keys::SPACE.to_string(),
        "left" => keys::LEFT.to_string(),
        "right" => keys::RIGHT.to_string(),
        "up" => keys::UP.to_string(),
        "down" => keys::DOWN.to_string(),
        "home" => keys::HOME.to_string(),
        "end" => keys::END.to_string(),
        "pageup" | "pgup" => keys::PAGE_UP.to_string(),
        "pagedown" | "pgdn" => keys::PAGE_DOWN.to_string(),
        _ => return None,
    };

    Some(EditorEvent::KeyPress {
        key,
        modifiers: Modifiers::default(),
    })
}

/// Parses a key sequence with modifiers (e.g., "C-a", "S-Tab").
fn parse_modified_key(seq: &str) -> Option<EditorEvent> {
    // Check for Ctrl+key pattern (C-x or Ctrl-x)
    if seq.starts_with("C-") || seq.starts_with("Ctrl-") {
        let key_part = if let Some(stripped) = seq.strip_prefix("C-") {
            stripped
        } else {
            &seq[5..]
        };

        if key_part.len() == 1 {
            let ch = key_part.chars().next().expect("key_part has length 1");
            return Some(EditorEvent::KeyPress {
                key: ch.to_lowercase().to_string(),
                modifiers: Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            });
        }
    }

    // Check for Alt+key pattern (A-x, M-x, Alt-x, Meta-x)
    if seq.starts_with("A-")
        || seq.starts_with("M-")
        || seq.starts_with("Alt-")
        || seq.starts_with("Meta-")
    {
        let key_part = if seq.starts_with("A-") || seq.starts_with("M-") {
            &seq[2..]
        } else if let Some(stripped) = seq.strip_prefix("Alt-") {
            stripped
        } else {
            &seq[5..]
        };

        if key_part.len() == 1 {
            let ch = key_part.chars().next().expect("key_part has length 1");
            return Some(EditorEvent::KeyPress {
                key: ch.to_lowercase().to_string(),
                modifiers: Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            });
        }
    }

    // Check for Shift+key pattern (S-Tab, Shift-Tab)
    if seq.starts_with("S-") || seq.starts_with("Shift-") {
        let key_part = if let Some(stripped) = seq.strip_prefix("S-") {
            stripped
        } else {
            &seq[6..]
        };

        match key_part.to_lowercase().as_str() {
            "tab" => {
                return Some(EditorEvent::KeyPress {
                    key: keys::TAB.to_string(),
                    modifiers: Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    },
                });
            },
            _ => {
                // For single characters with shift
                if key_part.len() == 1 {
                    let ch = key_part.chars().next().expect("key_part has length 1");
                    return Some(EditorEvent::KeyPress {
                        key: ch.to_uppercase().to_string(),
                        modifiers: Modifiers {
                            shift: true,
                            ..Modifiers::default()
                        },
                    });
                }
            },
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_characters() {
        let events = parse_sequence("abc");
        assert_eq!(events.len(), 3);

        for (i, ch) in "abc".chars().enumerate() {
            if let EditorEvent::KeyPress { key, modifiers } = &events[i] {
                assert_eq!(*key, ch.to_string());
                assert!(!modifiers.control);
                assert!(!modifiers.alt);
                assert!(!modifiers.shift);
            } else {
                panic!("Expected KeyPress event");
            }
        }
    }

    #[test]
    fn parse_special_keys() {
        let events = parse_sequence("<Esc><Enter><Tab>");
        assert_eq!(events.len(), 3);

        if let EditorEvent::KeyPress { key, .. } = &events[0] {
            assert_eq!(*key, keys::ESCAPE);
        }

        if let EditorEvent::KeyPress { key, .. } = &events[1] {
            assert_eq!(*key, keys::ENTER);
        }

        if let EditorEvent::KeyPress { key, .. } = &events[2] {
            assert_eq!(*key, keys::TAB);
        }
    }

    #[test]
    fn parse_mixed_content() {
        let events = parse_sequence("abc<Esc>def<Enter>");
        assert_eq!(events.len(), 8); // a, b, c, Esc, d, e, f, Enter
    }

    #[test]
    fn parse_modified_keys() {
        // Ctrl+A
        let events = parse_sequence("<C-a>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.control);
        }

        // Alt+X
        let events = parse_sequence("<A-x>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.alt);
        }

        // Shift+Tab
        let events = parse_sequence("<S-Tab>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, key } = &events[0] {
            assert!(modifiers.shift);
            assert_eq!(*key, keys::TAB);
        }
    }

    #[test]
    fn parse_incomplete_brackets() {
        let events = parse_sequence("a<bc");
        // When '<' is not closed, '<' and subsequent chars are treated as literals
        assert_eq!(events.len(), 4); // a, <, b, c
    }

    #[test]
    fn parse_arrow_keys() {
        let events = parse_sequence("<Left><Right><Up><Down>");
        assert_eq!(events.len(), 4);

        if let EditorEvent::KeyPress { key, .. } = &events[0] {
            assert_eq!(*key, keys::LEFT);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[1] {
            assert_eq!(*key, keys::RIGHT);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[2] {
            assert_eq!(*key, keys::UP);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[3] {
            assert_eq!(*key, keys::DOWN);
        }
    }

    #[test]
    fn parse_navigation_keys() {
        let events = parse_sequence("<Home><End><PageUp><PageDown>");
        assert_eq!(events.len(), 4);

        if let EditorEvent::KeyPress { key, .. } = &events[0] {
            assert_eq!(*key, keys::HOME);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[1] {
            assert_eq!(*key, keys::END);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[2] {
            assert_eq!(*key, keys::PAGE_UP);
        }
        if let EditorEvent::KeyPress { key, .. } = &events[3] {
            assert_eq!(*key, keys::PAGE_DOWN);
        }
    }

    #[test]
    fn parse_vim_sequence() {
        let events = parse_sequence("iHello<Esc>:wq<Enter>");
        assert_eq!(events.len(), 11); // i, H, e, l, l, o, Esc, :, w, q, Enter

        // Check first char is 'i'
        if let EditorEvent::KeyPress { key, .. } = &events[0] {
            assert_eq!(*key, "i");
        }

        // Check Escape
        if let EditorEvent::KeyPress { key, .. } = &events[6] {
            assert_eq!(*key, keys::ESCAPE);
        }

        // Check Enter at the end
        if let EditorEvent::KeyPress { key, .. } = &events[10] {
            assert_eq!(*key, keys::ENTER);
        }
    }
}
