use serde::{Deserialize, Serialize};

/// Represents a keyboard key or key combination
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Key {
    /// Special named keys
    Named(NamedKey),

    /// Modified keys like Ctrl+A
    Modified(ModifiedKey),

    /// Single character
    Char(char),

    /// Key sequence like "dd" or "gg"
    Sequence(String),
}

/// Special keys that have names
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum NamedKey {
    Esc,
    Enter,
    Tab,
    Space,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Modified key combinations
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum ModifiedKey {
    Ctrl(char),
    Alt(char),
    Shift(char),
    #[serde(rename = "Ctrl+Shift")]
    CtrlShift(char),
    #[serde(rename = "Ctrl+Alt")]
    CtrlAlt(char),
}

impl Key {
    /// Check if this key could be the start of a sequence
    pub fn could_be_sequence_start(&self, other: &str) -> bool {
        match self {
            Key::Sequence(seq) => other.starts_with(seq),
            Key::Char(ch) => other.starts_with(*ch),
            _ => false,
        }
    }
}

impl From<char> for Key {
    fn from(ch: char) -> Self {
        Key::Char(ch)
    }
}

impl From<NamedKey> for Key {
    fn from(named: NamedKey) -> Self {
        Key::Named(named)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_serialization() {
        // Test RON serialization
        let key = Key::Char('a');
        let serialized = ron::to_string(&key).expect("Failed to serialize Char key");
        assert_eq!(serialized, "Char('a')");

        let key = Key::Named(NamedKey::Esc);
        let serialized = ron::to_string(&key).expect("Failed to serialize Named key");
        assert_eq!(serialized, "Named(Esc)");

        let key = Key::Modified(ModifiedKey::Ctrl('s'));
        let serialized = ron::to_string(&key).expect("Failed to serialize Modified key");
        assert_eq!(serialized, "Modified(Ctrl('s'))");
    }

    #[test]
    fn test_key_deserialization() {
        // Test that we can parse various key formats
        let key: Key = ron::from_str("Char('a')").expect("Failed to deserialize char key");
        assert_eq!(key, Key::Char('a'));

        let key: Key = ron::from_str("Named(Esc)").expect("Failed to deserialize Esc key");
        assert_eq!(key, Key::Named(NamedKey::Esc));

        let key: Key =
            ron::from_str("Modified(Ctrl('s'))").expect("Failed to deserialize Ctrl+s key");
        assert_eq!(key, Key::Modified(ModifiedKey::Ctrl('s')));

        let key: Key =
            ron::from_str("Sequence(\"dd\")").expect("Failed to deserialize sequence key");
        assert_eq!(key, Key::Sequence("dd".to_string()));
    }
}
