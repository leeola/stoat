use stoat_core::input::{Key, ModifiedKey, NamedKey};

/// Convert an Iced keyboard key to a Stoat key
pub fn convert_key(
    iced_key: iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> Option<Key> {
    use iced::keyboard::key;

    match iced_key {
        // Named keys
        iced::keyboard::Key::Named(named) => match named {
            key::Named::Escape => Some(Key::Named(NamedKey::Esc)),
            key::Named::Enter => Some(Key::Named(NamedKey::Enter)),
            key::Named::Tab => Some(Key::Named(NamedKey::Tab)),
            key::Named::Space => Some(Key::Named(NamedKey::Space)),
            key::Named::Backspace => Some(Key::Named(NamedKey::Backspace)),
            key::Named::Delete => Some(Key::Named(NamedKey::Delete)),
            key::Named::ArrowUp => Some(Key::Named(NamedKey::Up)),
            key::Named::ArrowDown => Some(Key::Named(NamedKey::Down)),
            key::Named::ArrowLeft => Some(Key::Named(NamedKey::Left)),
            key::Named::ArrowRight => Some(Key::Named(NamedKey::Right)),
            key::Named::Home => Some(Key::Named(NamedKey::Home)),
            key::Named::End => Some(Key::Named(NamedKey::End)),
            key::Named::PageUp => Some(Key::Named(NamedKey::PageUp)),
            key::Named::PageDown => Some(Key::Named(NamedKey::PageDown)),
            _ => None,
        },

        // Character keys
        iced::keyboard::Key::Character(s) => {
            if let Some(ch) = s.chars().next() {
                if s.len() == 1 {
                    // Check for modified keys
                    if modifiers.control() && !modifiers.shift() && !modifiers.alt() {
                        Some(Key::Modified(ModifiedKey::Ctrl(ch)))
                    } else if modifiers.alt() && !modifiers.control() && !modifiers.shift() {
                        Some(Key::Modified(ModifiedKey::Alt(ch)))
                    } else if modifiers.shift() && !modifiers.control() && !modifiers.alt() {
                        // Shift modifier - create a Shift modified key
                        Some(Key::Modified(ModifiedKey::Shift(ch)))
                    } else if modifiers.control() && modifiers.shift() && !modifiers.alt() {
                        Some(Key::Modified(ModifiedKey::CtrlShift(ch)))
                    } else if modifiers.control() && modifiers.alt() && !modifiers.shift() {
                        Some(Key::Modified(ModifiedKey::CtrlAlt(ch)))
                    } else if !modifiers.control() && !modifiers.alt() && !modifiers.shift() {
                        // No modifiers
                        Some(Key::Char(ch))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        },

        // Unidentified keys
        iced::keyboard::Key::Unidentified => None,
    }
}
