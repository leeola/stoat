use crate::host::ClipboardHost;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    io::{self, Write},
    sync::Mutex,
};

/// Production [`ClipboardHost`] backed by a persistent [`arboard::Clipboard`].
///
/// The handle is created lazily on first use and retained for the life of the
/// process. On X11 the clipboard contents are served by the owning process
/// only while a handle lives, so a fresh handle per call drops selection
/// ownership the instant it returns and loses the copy unless a clipboard
/// manager races to grab it. Retaining one handle keeps the copy alive. It
/// also avoids arboard's debug-build Drop warning, which prints raw to stderr
/// when a handle drops within 100ms of a write.
///
/// Lazy creation preserves fail-late behavior. Machines without a display
/// server (CI, headless servers) surface the failure on the first clipboard
/// use rather than at process startup.
#[derive(Default)]
pub struct LocalClipboard {
    handle: Mutex<Option<arboard::Clipboard>>,
}

impl LocalClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ClipboardHost for LocalClipboard {
    fn set(&self, text: &str) -> io::Result<()> {
        let mut handle = self.handle.lock().expect("poisoned");

        if let Some(mut clipboard) = handle.take()
            && clipboard.set_text(text).is_ok()
        {
            *handle = Some(clipboard);
            return Ok(());
        }

        // Either no handle was cached or the cached one failed to write (a
        // stale display connection). Construct a fresh handle and retry once.
        let mut clipboard = arboard::Clipboard::new().map_err(io::Error::other)?;
        clipboard.set_text(text).map_err(io::Error::other)?;
        *handle = Some(clipboard);
        Ok(())
    }

    fn get(&self) -> io::Result<Option<String>> {
        let mut handle = self.handle.lock().expect("poisoned");

        let mut clipboard = match handle.take() {
            Some(clipboard) => clipboard,
            None => match arboard::Clipboard::new() {
                Ok(clipboard) => clipboard,
                Err(_) => return Ok(None),
            },
        };

        match clipboard.get_text() {
            Ok(text) => {
                *handle = Some(clipboard);
                Ok(Some(text))
            },
            Err(arboard::Error::ContentNotAvailable) => {
                *handle = Some(clipboard);
                Ok(None)
            },
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
