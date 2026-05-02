use std::io;

/// System clipboard write surface.
///
/// Production code routes clipboard writes through this trait so tests
/// can install [`crate::host::FakeClipboard`] without leaking into the
/// real OS clipboard. UTF-8-only by design: callers serialize into a
/// `&str` before invoking [`Self::set`].
pub trait ClipboardHost: Send + Sync {
    /// Replaces the system clipboard contents with `text`.
    fn set(&self, text: &str) -> io::Result<()>;
}

/// No-op [`ClipboardHost`] used when no real clipboard is needed (or
/// available). Logs the would-be write at trace level and returns
/// success so call sites can ignore the absence of a real clipboard.
pub struct NoopClipboard;

impl ClipboardHost for NoopClipboard {
    fn set(&self, text: &str) -> io::Result<()> {
        tracing::trace!(
            target: "stoat::host::clipboard",
            len = text.len(),
            "clipboard set ignored (NoopClipboard)"
        );
        Ok(())
    }
}
