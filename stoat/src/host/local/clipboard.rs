use crate::host::ClipboardHost;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::io::{self, Write};

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

    fn get(&self) -> io::Result<Option<String>> {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return Ok(None);
        };
        match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(io::Error::other(err)),
        }
    }

    fn osc52_emit(&self, text: &str) -> io::Result<()> {
        let payload = STANDARD.encode(text.as_bytes());
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\x1b]52;c;")?;
        stdout.write_all(payload.as_bytes())?;
        stdout.write_all(b"\x1b\\")?;
        stdout.flush()
    }
}
