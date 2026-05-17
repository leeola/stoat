//! In-process register store for yank/paste and (later) macros.
//!
//! Backs the unnamed register (`"`) plus helix-style named
//! registers (`a-z`). System / primary clipboard variants are
//! handled separately by [`crate::host::ClipboardHost`] -- the
//! action layer routes those operations directly rather than
//! going through this store.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Register {
    Unnamed,
    Named(char),
    /// System clipboard, addressed by `*` or `+` in the register
    /// chord. Writes route to [`crate::host::ClipboardHost::set`],
    /// reads to [`crate::host::ClipboardHost::get`].
    Clipboard,
    /// Last search query. Writes are no-ops; reads return
    /// `Stoat::last_search.query`.
    Search,
    /// Blackhole register. Writes are silently swallowed; reads
    /// return nothing. Use as a yank/delete destination when the
    /// caller does not want to clobber the unnamed register.
    Blackhole,
    /// Selection index. Writes are no-ops; pastes expand to one
    /// "1", "2", ... per selection in start-offset order.
    SelectionIndex,
    /// Last inserted text recorded by [`crate::app::Stoat::editor_insert`].
    /// Writes are no-ops; reads return the most recent insert.
    LastInsert,
}

#[derive(Debug, Default)]
pub struct RegisterStore {
    unnamed: Option<String>,
    named: HashMap<char, String>,
}

impl RegisterStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write `content` to the unnamed or a named register. Special
    /// registers (clipboard, search, blackhole, selection index,
    /// last insert) are filtered by the action layer before
    /// reaching this store and silently no-op when passed through.
    pub fn write(&mut self, register: Register, content: String) {
        match register {
            Register::Unnamed => self.unnamed = Some(content),
            Register::Named(c) => {
                self.named.insert(c, content);
            },
            Register::Clipboard
            | Register::Search
            | Register::Blackhole
            | Register::SelectionIndex
            | Register::LastInsert => {},
        }
    }

    /// Read the unnamed or a named register's content. Special
    /// registers are routed through the action layer to their
    /// backing state and bypass this store; reading them here
    /// always returns `None`.
    pub fn read(&self, register: Register) -> Option<&str> {
        match register {
            Register::Unnamed => self.unnamed.as_deref(),
            Register::Named(c) => self.named.get(&c).map(String::as_str),
            Register::Clipboard
            | Register::Search
            | Register::Blackhole
            | Register::SelectionIndex
            | Register::LastInsert => None,
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
