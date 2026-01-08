use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Key {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    pub fn char(c: char) -> Self {
        Self::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub fn code(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    pub fn esc() -> Self {
        Self::code(KeyCode::Esc)
    }

    pub fn enter() -> Self {
        Self::code(KeyCode::Enter)
    }

    pub fn tab() -> Self {
        Self::code(KeyCode::Tab)
    }

    pub fn backspace() -> Self {
        Self::code(KeyCode::Backspace)
    }

    pub fn f(n: u8) -> Self {
        Self::code(KeyCode::F(n))
    }

    pub fn up() -> Self {
        Self::code(KeyCode::Up)
    }

    pub fn down() -> Self {
        Self::code(KeyCode::Down)
    }

    pub fn left() -> Self {
        Self::code(KeyCode::Left)
    }

    pub fn right() -> Self {
        Self::code(KeyCode::Right)
    }

    pub fn ctrl(mut self) -> Self {
        self.modifiers |= KeyModifiers::CONTROL;
        self
    }

    pub fn shift(mut self) -> Self {
        self.modifiers |= KeyModifiers::SHIFT;
        self
    }

    pub fn alt(mut self) -> Self {
        self.modifiers |= KeyModifiers::ALT;
        self
    }

    pub fn super_key(mut self) -> Self {
        self.modifiers |= KeyModifiers::SUPER;
        self
    }

    pub fn meta(mut self) -> Self {
        self.modifiers |= KeyModifiers::META;
        self
    }

    pub fn matches(&self, event: &KeyEvent) -> bool {
        self.code == event.code && self.modifiers == event.modifiers
    }
}

impl From<KeyEvent> for Key {
    fn from(event: KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }
}

impl From<Key> for KeyEvent {
    fn from(key: Key) -> Self {
        KeyEvent::new(key.code, key.modifiers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_key() {
        let key = Key::char('j');
        assert_eq!(key.code, KeyCode::Char('j'));
        assert_eq!(key.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn ctrl_modifier() {
        let key = Key::char('c').ctrl();
        assert_eq!(key.code, KeyCode::Char('c'));
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn multiple_modifiers() {
        let key = Key::char('k').ctrl().shift();
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn matches_crossterm_event() {
        let key = Key::char('j').ctrl();
        let event = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(key.matches(&event));
    }

    #[test]
    fn from_crossterm_event() {
        let event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        let key = Key::from(event);
        assert_eq!(key.code, KeyCode::Char('x'));
        assert!(key.modifiers.contains(KeyModifiers::ALT));
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
    }
}
