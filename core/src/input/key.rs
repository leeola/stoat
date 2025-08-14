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

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Char(c) => write!(f, "{c}"),
            Key::Named(named) => write!(f, "<{}>", named.as_str()),
            Key::Modified(modified) => write!(f, "{modified}"),
            Key::Sequence(seq) => write!(f, "{seq}"),
        }
    }
}

impl NamedKey {
    /// Get the string representation for use in notation
    fn as_str(&self) -> &str {
        match self {
            NamedKey::Esc => "Esc",
            NamedKey::Enter => "Enter",
            NamedKey::Tab => "Tab",
            NamedKey::Space => "Space",
            NamedKey::Backspace => "BS",
            NamedKey::Delete => "Del",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PgUp",
            NamedKey::PageDown => "PgDn",
        }
    }
}

impl std::fmt::Display for NamedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::fmt::Display for ModifiedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModifiedKey::Shift(c) => {
                // Shift keys display as their shifted character
                let display_char = match *c {
                    // Symbols
                    '/' => '?',
                    '1' => '!',
                    '2' => '@',
                    '3' => '#',
                    '4' => '$',
                    '5' => '%',
                    '6' => '^',
                    '7' => '&',
                    '8' => '*',
                    '9' => '(',
                    '0' => ')',
                    '-' => '_',
                    '=' => '+',
                    '[' => '{',
                    ']' => '}',
                    '\\' => '|',
                    ';' => ':',
                    '\'' => '"',
                    ',' => '<',
                    '.' => '>',
                    '`' => '~',

                    // Letters - convert to uppercase
                    'a'..='z' => c.to_ascii_uppercase(),

                    // Already uppercase or unknown - show in Vim notation
                    _ => return write!(f, "<S-{c}>"),
                };
                write!(f, "{display_char}")
            },
            ModifiedKey::Ctrl(c) => write!(f, "<C-{c}>"),
            ModifiedKey::Alt(c) => write!(f, "<A-{c}>"),
            ModifiedKey::CtrlShift(c) => write!(f, "<C-S-{c}>"),
            ModifiedKey::CtrlAlt(c) => write!(f, "<C-A-{c}>"),
        }
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

    #[test]
    fn test_vim_notation_display() {
        // Test Vim-style notation for modified keys
        assert_eq!(Key::Modified(ModifiedKey::Ctrl('a')).to_string(), "<C-a>");
        assert_eq!(Key::Modified(ModifiedKey::Alt('x')).to_string(), "<A-x>");
        assert_eq!(
            Key::Modified(ModifiedKey::CtrlShift('z')).to_string(),
            "<C-S-z>"
        );
        assert_eq!(
            Key::Modified(ModifiedKey::CtrlAlt('f')).to_string(),
            "<C-A-f>"
        );

        // Test named keys with angle brackets
        assert_eq!(Key::Named(NamedKey::Esc).to_string(), "<Esc>");
        assert_eq!(Key::Named(NamedKey::Enter).to_string(), "<Enter>");
        assert_eq!(Key::Named(NamedKey::Tab).to_string(), "<Tab>");
        assert_eq!(Key::Named(NamedKey::Backspace).to_string(), "<BS>");
        assert_eq!(Key::Named(NamedKey::Delete).to_string(), "<Del>");

        // Plain characters remain unchanged
        assert_eq!(Key::Char('a').to_string(), "a");
        assert_eq!(Key::Char('1').to_string(), "1");
    }

    #[test]
    fn test_shift_key_display() {
        // Shift + letters become uppercase
        assert_eq!(Key::Modified(ModifiedKey::Shift('a')).to_string(), "A");
        assert_eq!(Key::Modified(ModifiedKey::Shift('z')).to_string(), "Z");
        assert_eq!(Key::Modified(ModifiedKey::Shift('m')).to_string(), "M");

        // Shift + symbols show their shifted character
        assert_eq!(Key::Modified(ModifiedKey::Shift('/')).to_string(), "?");
        assert_eq!(Key::Modified(ModifiedKey::Shift('1')).to_string(), "!");
        assert_eq!(Key::Modified(ModifiedKey::Shift('2')).to_string(), "@");
        assert_eq!(Key::Modified(ModifiedKey::Shift('3')).to_string(), "#");
        assert_eq!(Key::Modified(ModifiedKey::Shift('4')).to_string(), "$");
        assert_eq!(Key::Modified(ModifiedKey::Shift('5')).to_string(), "%");
        assert_eq!(Key::Modified(ModifiedKey::Shift('6')).to_string(), "^");
        assert_eq!(Key::Modified(ModifiedKey::Shift('7')).to_string(), "&");
        assert_eq!(Key::Modified(ModifiedKey::Shift('8')).to_string(), "*");
        assert_eq!(Key::Modified(ModifiedKey::Shift('9')).to_string(), "(");
        assert_eq!(Key::Modified(ModifiedKey::Shift('0')).to_string(), ")");
        assert_eq!(Key::Modified(ModifiedKey::Shift('-')).to_string(), "_");
        assert_eq!(Key::Modified(ModifiedKey::Shift('=')).to_string(), "+");
        assert_eq!(Key::Modified(ModifiedKey::Shift('[')).to_string(), "{");
        assert_eq!(Key::Modified(ModifiedKey::Shift(']')).to_string(), "}");
        assert_eq!(Key::Modified(ModifiedKey::Shift('\\')).to_string(), "|");
        assert_eq!(Key::Modified(ModifiedKey::Shift(';')).to_string(), ":");
        assert_eq!(Key::Modified(ModifiedKey::Shift('\'')).to_string(), "\"");
        assert_eq!(Key::Modified(ModifiedKey::Shift(',')).to_string(), "<");
        assert_eq!(Key::Modified(ModifiedKey::Shift('.')).to_string(), ">");
        assert_eq!(Key::Modified(ModifiedKey::Shift('`')).to_string(), "~");

        // Unknown shift combinations fall back to Vim notation
        assert_eq!(Key::Modified(ModifiedKey::Shift('$')).to_string(), "<S-$>");
    }

    #[test]
    fn test_display_vs_debug_format() {
        let shift_slash = Key::Modified(ModifiedKey::Shift('/'));
        let ctrl_a = Key::Modified(ModifiedKey::Ctrl('a'));
        let esc_key = Key::Named(NamedKey::Esc);

        // Display format is user-friendly
        assert_eq!(shift_slash.to_string(), "?");
        assert_eq!(ctrl_a.to_string(), "<C-a>");
        assert_eq!(esc_key.to_string(), "<Esc>");

        // Debug format shows internal structure
        assert_eq!(format!("{shift_slash:?}"), "Modified(Shift('/'))");
        assert_eq!(format!("{ctrl_a:?}"), "Modified(Ctrl('a'))");
        assert_eq!(format!("{esc_key:?}"), "Named(Esc)");
    }
}
