use crate::host::ClipboardHost;
use std::io;

/// Production [`ClipboardHost`] backed by [`arboard::Clipboard`].
///
/// Each [`set`](ClipboardHost::set) call constructs a fresh
/// `arboard::Clipboard` so the host is stateless and fails late --
/// machines without a display server (CI, headless servers) surface
/// the failure on the first clipboard write rather than at process
/// startup.
pub struct LocalClipboard;

impl ClipboardHost for LocalClipboard {
    fn set(&self, text: &str) -> io::Result<()> {
        let mut clipboard = arboard::Clipboard::new().map_err(io::Error::other)?;
        clipboard.set_text(text).map_err(io::Error::other)
    }
}
