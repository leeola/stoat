use crate::host::ClipboardHost;
use std::{io, sync::Mutex};

/// In-memory [`ClipboardHost`] for tests. Records every
/// [`set`](ClipboardHost::set) call into an internal buffer that
/// [`writes`](Self::writes) returns in call order.
pub struct FakeClipboard {
    writes: Mutex<Vec<String>>,
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
        }
    }

    /// Snapshots the recorded write log in call order. Each entry is
    /// the `text` argument from a [`ClipboardHost::set`] call.
    pub fn writes(&self) -> Vec<String> {
        self.writes.lock().expect("poisoned").clone()
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
    }
}
