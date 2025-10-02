use gpui::{App, Window};
use tracing::info;

/// Represents a single input event that can be simulated
#[derive(Debug, Clone)]
pub(crate) enum InputEvent {
    /// A regular character to type
    Character(char),
    /// A special key like Escape, Enter, etc.
    SpecialKey(SpecialKey),
    /// A control key combination (e.g., Ctrl+F)
    ControlKey(char),
}

#[derive(Debug, Clone)]
pub(crate) enum SpecialKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Parse an input sequence string into a series of input events
/// Supports vim-like notation:
/// - Regular characters: "abc" -> types 'a', 'b', 'c'
/// - Special keys in angle brackets: "<Esc>", "<Enter>", "<Tab>"
/// - Arrow keys: "<Left>", "<Right>", "<Up>", "<Down>"
/// - Combined: "iHello<Esc>" -> 'i', 'H', 'e', 'l', 'l', 'o', then Escape
pub(crate) fn parse_input_sequence(input: &str) -> Vec<InputEvent> {
    let mut events = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            // Parse special key notation
            let mut key_name = String::new();
            let mut found_end = false;

            for inner in chars.by_ref() {
                if inner == '>' {
                    found_end = true;
                    break;
                }
                key_name.push(inner);
            }

            if found_end {
                // Check if it's a control key combination first
                if let Some(control_char) = parse_control_key(&key_name) {
                    events.push(InputEvent::ControlKey(control_char));
                } else if let Some(special_key) = parse_special_key(&key_name) {
                    events.push(InputEvent::SpecialKey(special_key));
                } else {
                    // Unknown special key, treat as literal characters
                    events.push(InputEvent::Character('<'));
                    for ch in key_name.chars() {
                        events.push(InputEvent::Character(ch));
                    }
                    events.push(InputEvent::Character('>'));
                }
            } else {
                // Unclosed bracket, treat as literal
                events.push(InputEvent::Character('<'));
                for ch in key_name.chars() {
                    events.push(InputEvent::Character(ch));
                }
            }
        } else {
            events.push(InputEvent::Character(ch));
        }
    }

    events
}

/// Parse control key notation like "C-f", "ctrl-f", or "Ctrl-F"
fn parse_control_key(name: &str) -> Option<char> {
    let lower = name.to_lowercase();

    // Check for vim-style notation: C-x
    if lower.starts_with("c-") && lower.len() == 3 {
        return lower.chars().nth(2);
    }

    // Check for full notation: ctrl-x or control-x
    if lower.starts_with("ctrl-") && lower.len() == 6 {
        return lower.chars().nth(5);
    }

    if lower.starts_with("control-") && lower.len() == 9 {
        return lower.chars().nth(8);
    }

    None
}

fn parse_special_key(name: &str) -> Option<SpecialKey> {
    match name.to_lowercase().as_str() {
        "esc" | "escape" => Some(SpecialKey::Escape),
        "enter" | "return" | "cr" => Some(SpecialKey::Enter),
        "tab" => Some(SpecialKey::Tab),
        "backspace" | "bs" => Some(SpecialKey::Backspace),
        "delete" | "del" => Some(SpecialKey::Delete),
        "left" => Some(SpecialKey::Left),
        "right" => Some(SpecialKey::Right),
        "up" => Some(SpecialKey::Up),
        "down" => Some(SpecialKey::Down),
        "home" => Some(SpecialKey::Home),
        "end" => Some(SpecialKey::End),
        "pageup" | "pgup" => Some(SpecialKey::PageUp),
        "pagedown" | "pgdown" => Some(SpecialKey::PageDown),
        _ => None,
    }
}

/// Simulate the input sequence by dispatching appropriate events to the window
pub fn simulate_input_sequence(input: &str, window: &mut Window, cx: &mut App) {
    info!("Simulating input sequence: {}", input);

    let events = parse_input_sequence(input);

    for event in events {
        match event {
            InputEvent::Character(ch) => {
                // For regular characters in insert mode, dispatch as text input
                // The modal system will handle whether we're in insert mode
                if ch == 'i' || ch == 'I' || ch == 'a' || ch == 'A' || ch == 'o' || ch == 'O' {
                    // Modal commands - dispatch as keystrokes
                    dispatch_character_as_keystroke(ch, window, cx);
                } else if ch == 'h' || ch == 'j' || ch == 'k' || ch == 'l' {
                    // Movement commands in normal mode - dispatch as keystrokes
                    dispatch_character_as_keystroke(ch, window, cx);
                } else if ch == 'd' || ch == 'x' || ch == 'c' || ch == 'y' || ch == 'p' {
                    // Operator commands - dispatch as keystrokes
                    dispatch_character_as_keystroke(ch, window, cx);
                } else {
                    // Regular text - dispatch as keystroke (modal system will handle)
                    dispatch_character_as_keystroke(ch, window, cx);
                }
            },
            InputEvent::SpecialKey(key) => {
                dispatch_special_key(key, window, cx);
            },
            InputEvent::ControlKey(ch) => {
                dispatch_control_key(ch, window, cx);
            },
        }
    }
}

fn dispatch_character_as_keystroke(ch: char, window: &mut Window, cx: &mut App) {
    use gpui::Keystroke;

    let key = ch.to_string();
    let keystroke = Keystroke {
        modifiers: gpui::Modifiers::none(),
        key,
        key_char: Some(ch.to_string()),
    };

    window.dispatch_keystroke(keystroke, cx);
}

fn dispatch_control_key(ch: char, window: &mut Window, cx: &mut App) {
    use gpui::{Keystroke, Modifiers};

    let keystroke = Keystroke {
        modifiers: Modifiers::control(),
        key: ch.to_string(),
        key_char: Some(ch.to_string()),
    };

    window.dispatch_keystroke(keystroke, cx);
}

fn dispatch_special_key(key: SpecialKey, window: &mut Window, cx: &mut App) {
    use gpui::Keystroke;

    let (key_str, key_char) = match key {
        SpecialKey::Escape => ("escape", None),
        SpecialKey::Enter => ("enter", Some("\n".to_string())),
        SpecialKey::Tab => ("tab", Some("\t".to_string())),
        SpecialKey::Backspace => ("backspace", None),
        SpecialKey::Delete => ("delete", None),
        SpecialKey::Left => ("left", None),
        SpecialKey::Right => ("right", None),
        SpecialKey::Up => ("up", None),
        SpecialKey::Down => ("down", None),
        SpecialKey::Home => ("home", None),
        SpecialKey::End => ("end", None),
        SpecialKey::PageUp => ("pageup", None),
        SpecialKey::PageDown => ("pagedown", None),
    };

    let keystroke = Keystroke {
        modifiers: gpui::Modifiers::none(),
        key: key_str.to_string(),
        key_char,
    };

    window.dispatch_keystroke(keystroke, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_characters() {
        let events = parse_input_sequence("abc");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], InputEvent::Character('a')));
        assert!(matches!(events[1], InputEvent::Character('b')));
        assert!(matches!(events[2], InputEvent::Character('c')));
    }

    #[test]
    fn test_parse_special_keys() {
        let events = parse_input_sequence("<Esc><Enter><Tab>");
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            InputEvent::SpecialKey(SpecialKey::Escape)
        ));
        assert!(matches!(
            events[1],
            InputEvent::SpecialKey(SpecialKey::Enter)
        ));
        assert!(matches!(events[2], InputEvent::SpecialKey(SpecialKey::Tab)));
    }

    #[test]
    fn test_parse_mixed_sequence() {
        let events = parse_input_sequence("iHello<Esc>");
        assert_eq!(events.len(), 7);
        assert!(matches!(events[0], InputEvent::Character('i')));
        assert!(matches!(events[1], InputEvent::Character('H')));
        assert!(matches!(events[5], InputEvent::Character('o')));
        assert!(matches!(
            events[6],
            InputEvent::SpecialKey(SpecialKey::Escape)
        ));
    }

    #[test]
    fn test_parse_arrow_keys() {
        let events = parse_input_sequence("<Left><Right><Up><Down>");
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0],
            InputEvent::SpecialKey(SpecialKey::Left)
        ));
        assert!(matches!(
            events[1],
            InputEvent::SpecialKey(SpecialKey::Right)
        ));
        assert!(matches!(events[2], InputEvent::SpecialKey(SpecialKey::Up)));
        assert!(matches!(
            events[3],
            InputEvent::SpecialKey(SpecialKey::Down)
        ));
    }

    #[test]
    fn test_parse_control_keys_vim_style() {
        let events = parse_input_sequence("<C-f><C-b><C-d><C-u>");
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], InputEvent::ControlKey('f')));
        assert!(matches!(events[1], InputEvent::ControlKey('b')));
        assert!(matches!(events[2], InputEvent::ControlKey('d')));
        assert!(matches!(events[3], InputEvent::ControlKey('u')));
    }

    #[test]
    fn test_parse_control_keys_full_notation() {
        let events = parse_input_sequence("<ctrl-f><Ctrl-B><CTRL-d>");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], InputEvent::ControlKey('f')));
        assert!(matches!(events[1], InputEvent::ControlKey('b')));
        assert!(matches!(events[2], InputEvent::ControlKey('d')));
    }

    #[test]
    fn test_parse_control_keys_long_notation() {
        let events = parse_input_sequence("<control-a><Control-Z>");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], InputEvent::ControlKey('a')));
        assert!(matches!(events[1], InputEvent::ControlKey('z')));
    }

    #[test]
    fn test_parse_mixed_with_control_keys() {
        let events = parse_input_sequence("i<C-f>hello<Esc><ctrl-b>");
        assert_eq!(events.len(), 9);
        assert!(matches!(events[0], InputEvent::Character('i')));
        assert!(matches!(events[1], InputEvent::ControlKey('f')));
        assert!(matches!(events[2], InputEvent::Character('h')));
        assert!(matches!(events[3], InputEvent::Character('e')));
        assert!(matches!(events[4], InputEvent::Character('l')));
        assert!(matches!(events[5], InputEvent::Character('l')));
        assert!(matches!(events[6], InputEvent::Character('o')));
        assert!(matches!(
            events[7],
            InputEvent::SpecialKey(SpecialKey::Escape)
        ));
        assert!(matches!(events[8], InputEvent::ControlKey('b')));
    }

    #[test]
    fn test_parse_unknown_control_notation() {
        // Test that unknown control notation is treated as literal
        let events = parse_input_sequence("<C->");
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], InputEvent::Character('<')));
        assert!(matches!(events[1], InputEvent::Character('C')));
        assert!(matches!(events[2], InputEvent::Character('-')));
        assert!(matches!(events[3], InputEvent::Character('>')));
    }
}
