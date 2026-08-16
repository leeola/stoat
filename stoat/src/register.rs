//! In-process register store for yank/paste and (later) macros.
//!
//! Backs the unnamed register (`"`) plus a named register for every
//! char the special ones do not claim. System / primary clipboard variants are
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
    /// Selection contents. Reads yield one fragment per selection, the text
    /// that selection covers. Writes are refused, the text being the buffer's
    /// rather than the register's to hold.
    SelectionContents,
    /// The focused buffer's name, as the status row shows it. Writes are
    /// refused, since renaming a file is not something a paste does.
    DocumentPath,
    /// Command palette lines already run, newest first. Writes are refused,
    /// history being a record of what happened rather than a list to set.
    Command,
}

impl Register {
    /// The char that names this register in a chord and in a status message.
    ///
    /// The inverse of
    /// [`register_for_char`](crate::action_handlers::yank::register_for_char),
    /// and it has to stay in step with it. A reader who sees a register named
    /// in the status row types that same char to address it again.
    ///
    /// [`Self::Clipboard`] is the one variant two chars reach. It names itself
    /// `+`, the system clipboard, since that is the host it reads and writes.
    pub(crate) fn name(self) -> char {
        match self {
            Register::Unnamed => '"',
            Register::Named(c) => c,
            Register::Clipboard => '+',
            Register::Search => '/',
            Register::Blackhole => '_',
            Register::SelectionIndex => '#',
            Register::SelectionContents => '.',
            Register::DocumentPath => '%',
            Register::Command => ':',
        }
    }
}

/// What the clipboard's single string joins per-selection fragments with.
///
/// Both halves of the shadow read it, which keeps a write and the read that
/// validates it in step. Buffer text is LF whatever the file uses, and the read
/// side normalizes the host's answer before comparing, so a clipboard that
/// hands back CRLF still matches what was put there.
const CLIPBOARD_JOIN: &str = "\n";

#[derive(Debug, Default)]
pub(crate) struct RegisterStore {
    unnamed: Option<Vec<String>>,
    named: HashMap<char, Vec<String>>,
    /// Fragments last written to the system clipboard, kept so a multi-selection
    /// yank pastes back one fragment per selection.
    ///
    /// The system clipboard holds one string, which on its own flattens the
    /// fragments into a blob that pastes whole at every cursor. This is only
    /// ever an offer. [`Self::clipboard_shadow`] serves it while the host still
    /// holds the text it was joined into, so anything that changes the clipboard
    /// behind stoat's back falls back to that one string.
    clipboard_shadow: Option<Vec<String>>,
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record `fragments` as what the system clipboard now holds, and return
    /// the single string to put there.
    ///
    /// Returning the joined text rather than taking it keeps the two in step.
    /// The caller sends the host exactly the string [`Self::clipboard_shadow`]
    /// later compares against.
    pub(crate) fn shadow_clipboard(&mut self, fragments: Vec<String>) -> String {
        let joined = fragments.join(CLIPBOARD_JOIN);
        self.clipboard_shadow = Some(fragments);
        joined
    }

    /// The recorded fragments, while `contents` is still the text they were
    /// joined into.
    ///
    /// `contents` is the host's current answer, normalized. A mismatch means
    /// something replaced the clipboard since stoat wrote it, so the caller
    /// falls back to that one string and the fragments are not offered again.
    ///
    /// Two different writes joining to the same string are indistinguishable
    /// here, so a single selection holding "a\nb" reads back as the two
    /// fragments a previous yank left. The clipboard carries no structure to
    /// tell them apart, and re-splitting text the user did put there is the
    /// milder of the two wrong answers.
    pub(crate) fn clipboard_shadow(&self, contents: &str) -> Option<&[String]> {
        self.clipboard_shadow
            .as_ref()
            .filter(|fragments| fragments.join(CLIPBOARD_JOIN) == contents)
            .map(Vec::as_slice)
    }

    /// Write `fragments` to the unnamed or a named register, one entry
    /// per selection like Helix. Special registers (clipboard, search,
    /// blackhole, selection index, selection contents) are filtered by the
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
            | Register::SelectionContents
            | Register::DocumentPath
            | Register::Command => {},
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
            | Register::SelectionContents
            | Register::DocumentPath
            | Register::Command => None,
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
