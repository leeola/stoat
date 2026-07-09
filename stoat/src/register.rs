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
pub(crate) struct RegisterStore {
    unnamed: Option<Vec<String>>,
    named: HashMap<char, Vec<String>>,
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Write `fragments` to the unnamed or a named register, one entry
    /// per selection like Helix. Special registers (clipboard, search,
    /// blackhole, selection index, last insert) are filtered by the
    /// action layer before reaching this store and silently no-op when
    /// passed through.
    pub(crate) fn write(&mut self, register: Register, fragments: Vec<String>) {
        match register {
            Register::Unnamed => self.unnamed = Some(fragments),
            Register::Named(c) => {
                self.named.insert(c, fragments);
            },
            Register::Clipboard
            | Register::Search
            | Register::Blackhole
            | Register::SelectionIndex
            | Register::LastInsert => {},
        }
    }

    /// Read the unnamed or a named register's per-selection fragments.
    /// Special registers are routed through the action layer to their
    /// backing state and bypass this store. Reading one here always
    /// returns `None`.
    pub(crate) fn read(&self, register: Register) -> Option<&[String]> {
        match register {
            Register::Unnamed => self.unnamed.as_deref(),
            Register::Named(c) => self.named.get(&c).map(Vec::as_slice),
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

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn read_frags(store: &RegisterStore, register: Register) -> Option<Vec<String>> {
        store.read(register).map(<[String]>::to_vec)
    }

    #[test]
    fn write_then_read_unnamed() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, v(&["hello", "world"]));
        assert_eq!(
            read_frags(&store, Register::Unnamed),
            Some(v(&["hello", "world"]))
        );
    }

    #[test]
    fn write_overwrites_existing() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, v(&["first"]));
        store.write(Register::Unnamed, v(&["second"]));
        assert_eq!(read_frags(&store, Register::Unnamed), Some(v(&["second"])));
    }

    #[test]
    fn empty_store_returns_none() {
        let store = RegisterStore::new();
        assert_eq!(read_frags(&store, Register::Unnamed), None);
    }

    #[test]
    fn named_register_isolated_from_unnamed() {
        let mut store = RegisterStore::new();
        store.write(Register::Unnamed, v(&["anon"]));
        store.write(Register::Named('a'), v(&["alpha"]));
        assert_eq!(read_frags(&store, Register::Unnamed), Some(v(&["anon"])));
        assert_eq!(
            read_frags(&store, Register::Named('a')),
            Some(v(&["alpha"]))
        );
        assert_eq!(read_frags(&store, Register::Named('b')), None);
    }

    #[test]
    fn named_registers_isolated_from_each_other() {
        let mut store = RegisterStore::new();
        store.write(Register::Named('a'), v(&["alpha"]));
        store.write(Register::Named('b'), v(&["beta"]));
        assert_eq!(
            read_frags(&store, Register::Named('a')),
            Some(v(&["alpha"]))
        );
        assert_eq!(read_frags(&store, Register::Named('b')), Some(v(&["beta"])));
    }
}
