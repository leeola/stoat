use crate::host::ClipboardHost;
use std::{io, sync::Mutex};

/// In-memory [`ClipboardHost`] for tests. Records every
/// [`set`](ClipboardHost::set) call into an internal buffer that
/// [`writes`](Self::writes) returns in call order, and every
/// [`osc52_emit`](ClipboardHost::osc52_emit) call into a parallel
/// buffer surfaced via [`osc52_emits`](Self::osc52_emits).
pub struct FakeClipboard {
    writes: Mutex<Vec<String>>,
    osc52_emits: Mutex<Vec<String>>,
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
            osc52_emits: Mutex::new(Vec::new()),
        }
    }

    /// Snapshots the recorded write log in call order. Each entry is
    /// the `text` argument from a [`ClipboardHost::set`] call.
    pub fn writes(&self) -> Vec<String> {
        self.writes.lock().expect("poisoned").clone()
    }

    /// Snapshots the recorded OSC 52 emit log in call order. Each
    /// entry is the `text` argument from a
    /// [`ClipboardHost::osc52_emit`] call.
    pub fn osc52_emits(&self) -> Vec<String> {
        self.osc52_emits.lock().expect("poisoned").clone()
    }
}

impl Default for FakeClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardHost for FakeClipboard {
    fn set(&self, text: &str) -> io::Result<()> {
        self.writes.lock().expect("poisoned").push(text.to_owned());
        Ok(())
    }

    fn osc52_emit(&self, text: &str) -> io::Result<()> {
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
        cb.set("first").unwrap();
        cb.set("second").unwrap();
        cb.set("third").unwrap();
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
        cb.osc52_emit("alpha").unwrap();
        cb.osc52_emit("beta").unwrap();
        assert_eq!(cb.osc52_emits(), vec!["alpha", "beta"]);
        assert_eq!(cb.writes(), Vec::<String>::new());
    }
}
