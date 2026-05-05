//! In-process register store for yank/paste and (later) macros.
//!
//! Backs the unnamed register (`"`) plus helix-style named
//! registers (`a-z`). System / primary clipboard variants are
//! handled separately by [`crate::host::ClipboardHost`] -- the
//! action layer routes those operations directly rather than
//! going through this store.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Register {
    Unnamed,
    Named(char),
}

#[derive(Debug, Default)]
pub(crate) struct RegisterStore {
    unnamed: Option<String>,
    named: HashMap<char, String>,
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn write(&mut self, register: Register, content: String) {
        match register {
            Register::Unnamed => self.unnamed = Some(content),
            Register::Named(c) => {
                self.named.insert(c, content);
            },
        }
    }

    pub(crate) fn read(&self, register: Register) -> Option<&str> {
        match register {
            Register::Unnamed => self.unnamed.as_deref(),
            Register::Named(c) => self.named.get(&c).map(String::as_str),
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

    #[test]
    fn named_register_isolated_from_unnamed() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, "anon".to_string());
        store.write(Register::Named('a'), "alpha".to_string());
        assert_eq!(store.read(Register::Unnamed), Some("anon"));
        assert_eq!(store.read(Register::Named('a')), Some("alpha"));
        assert_eq!(store.read(Register::Named('b')), None);
    }

    #[test]
    fn named_registers_isolated_from_each_other() {
        let mut store = RegisterStore::new();
        store.write(Register::Named('a'), "alpha".to_string());
        store.write(Register::Named('b'), "beta".to_string());
        assert_eq!(store.read(Register::Named('a')), Some("alpha"));
        assert_eq!(store.read(Register::Named('b')), Some("beta"));
    }
}
