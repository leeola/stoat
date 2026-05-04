//! In-process register store for yank/paste and (later) macros.
//!
//! V1 backs a single unnamed register (`"`) which is the default
//! sink for `y` / `p` / `P`. Helix-style named (`a-z`) and system
//! / primary clipboard variants land alongside the
//! `select_register` action and `arboard` integration that need
//! them; defining them now without callers would be dead code.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Register {
    Unnamed,
}

#[derive(Debug, Default)]
pub(crate) struct RegisterStore {
    unnamed: Option<String>,
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn write(&mut self, register: Register, content: String) {
        match register {
            Register::Unnamed => self.unnamed = Some(content),
        }
    }

    pub(crate) fn read(&self, register: Register) -> Option<&str> {
        match register {
            Register::Unnamed => self.unnamed.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_unnamed() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, "hello".to_string());
        assert_eq!(store.read(Register::Unnamed), Some("hello"));
    }

    #[test]
    fn write_overwrites_existing() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, "first".to_string());
        store.write(Register::Unnamed, "second".to_string());
        assert_eq!(store.read(Register::Unnamed), Some("second"));
    }

    #[test]
    fn empty_store_returns_none() {
        let store = RegisterStore::new();
        assert_eq!(store.read(Register::Unnamed), None);
    }
}
