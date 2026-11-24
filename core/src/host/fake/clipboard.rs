use crate::host::{ClipboardHost, ClipboardKind};
use std::{io, sync::Mutex};

/// In-memory [`ClipboardHost`] for tests. Records every
/// [`set`](ClipboardHost::set) call into an internal buffer that
/// [`writes`](Self::writes) returns in call order, and every
/// [`osc52_emit`](ClipboardHost::osc52_emit) call into a parallel
/// buffer surfaced via [`osc52_emits`](Self::osc52_emits).
///
/// The two selections are recorded apart, the way a display server keeps them,
/// so a test can tell which one a yank reached.
pub struct FakeClipboard {
    system_writes: Mutex<Vec<String>>,
    primary_writes: Mutex<Vec<String>>,
    osc52_emits: Mutex<Vec<String>>,
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self {
            system_writes: Mutex::new(Vec::new()),
            primary_writes: Mutex::new(Vec::new()),
            osc52_emits: Mutex::new(Vec::new()),
        }
    }

    /// Snapshots the system clipboard's write log in call order. Each entry is
    /// the `text` argument from a [`ClipboardHost::set`] call.
    pub fn writes(&self) -> Vec<String> {
        self.writes_for(ClipboardKind::System)
    }

    /// Snapshots one selection's write log in call order.
    pub fn writes_for(&self, kind: ClipboardKind) -> Vec<String> {
        self.log(kind).lock().expect("poisoned").clone()
    }

    /// Snapshots the recorded OSC 52 emit log in call order. Each
    /// entry is the `text` argument from a
    /// [`ClipboardHost::osc52_emit`] call.
    pub fn osc52_emits(&self) -> Vec<String> {
        self.osc52_emits.lock().expect("poisoned").clone()
    }

    fn log(&self, kind: ClipboardKind) -> &Mutex<Vec<String>> {
        match kind {
            ClipboardKind::System => &self.system_writes,
            ClipboardKind::Primary => &self.primary_writes,
        }
    }
}

impl Default for FakeClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardHost for FakeClipboard {
    fn set(&self, kind: ClipboardKind, text: &str) -> io::Result<()> {
        self.log(kind)
            .lock()
            .expect("poisoned")
            .push(text.to_owned());
        Ok(())
    }

    fn get(&self, kind: ClipboardKind) -> io::Result<Option<String>> {
        Ok(self.log(kind).lock().expect("poisoned").last().cloned())
    }

    fn osc52_emit(&self, _kind: ClipboardKind, text: &str) -> io::Result<()> {
        self.osc52_emits
            .lock()
            .expect("poisoned")
            .push(text.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_writes_in_order() {
        let cb = FakeClipboard::new();
        cb.set(ClipboardKind::System, "first").unwrap();
        cb.set(ClipboardKind::System, "second").unwrap();
        cb.set(ClipboardKind::System, "third").unwrap();
        assert_eq!(cb.writes(), vec!["first", "second", "third"]);
    }

    #[test]
    fn empty_after_construction() {
        let cb = FakeClipboard::new();
        assert_eq!(cb.writes(), Vec::<String>::new());
        assert_eq!(cb.osc52_emits(), Vec::<String>::new());
    }

    #[test]
    fn records_osc52_emits_in_order() {
        let cb = FakeClipboard::new();
        cb.osc52_emit(ClipboardKind::System, "alpha").unwrap();
        cb.osc52_emit(ClipboardKind::System, "beta").unwrap();
        assert_eq!(cb.osc52_emits(), vec!["alpha", "beta"]);
        assert_eq!(cb.writes(), Vec::<String>::new());
    }
}
