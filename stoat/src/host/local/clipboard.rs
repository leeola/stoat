use crate::host::ClipboardHost;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    io::{self, Write},
    sync::Mutex,
};
use tokio::sync::mpsc::UnboundedSender;

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
pub struct LocalClipboard {
    handle: Mutex<Option<arboard::Clipboard>>,
    /// The UI thread's ordered byte channel, which is where the OSC 52 escape
    /// goes rather than to stdout directly.
    ///
    /// Yanks run on the event loop, and OSC 52 exists for SSH sessions, where
    /// writing a large payload means blocking that loop until the pipe drains.
    /// The escape also shares its fd with the frames the UI thread paints, so
    /// writing it here would let it land in the middle of one.
    ///
    /// `None` writes to stdout instead, which is what a host built outside the
    /// running UI has to do.
    osc52_sink: Option<UnboundedSender<Vec<u8>>>,
}

impl LocalClipboard {
    pub fn new(osc52_sink: Option<UnboundedSender<Vec<u8>>>) -> Self {
        Self {
            handle: Mutex::new(None),
            osc52_sink,
        }
    }
}

/// The OSC 52 set-clipboard escape carrying `text`.
///
/// Built rather than written so both the channel and the direct write emit the
/// same bytes. The payload is base64 because the escape's grammar has no way to
/// carry arbitrary text otherwise.
fn osc52_sequence(text: &str) -> Vec<u8> {
    let payload = STANDARD.encode(text.as_bytes());

    let mut sequence = Vec::with_capacity(payload.len() + 9);
    sequence.extend_from_slice(b"\x1b]52;c;");
    sequence.extend_from_slice(payload.as_bytes());
    sequence.extend_from_slice(b"\x1b\\");
    sequence
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
        let sequence = osc52_sequence(text);

        let Some(sink) = &self.osc52_sink else {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&sequence)?;
            return stdout.flush();
        };

        // One batch, so the escape cannot be split across two frames.
        sink.send(sequence)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "UI thread is gone"))
    }
}

#[cfg(test)]
mod tests {
    use super::{osc52_sequence, LocalClipboard};
    use crate::host::ClipboardHost;
    use tokio::sync::mpsc::unbounded_channel;

    /// The escape's wire format, which a terminal parses byte for byte: OSC,
    /// the 52 selection command, the `c` clipboard target, base64 text, then
    /// ST. Pinned literally, since nothing downstream of here would notice it
    /// drifting until a real terminal ignored the yank.
    #[test]
    fn the_escape_wraps_base64_text_in_osc_52() {
        assert_eq!(osc52_sequence("hello"), b"\x1b]52;c;aGVsbG8=\x1b\\");
    }

    /// A yank runs on the event loop, so the escape leaves as one message on
    /// the UI thread's channel rather than as a write the loop waits out.
    #[test]
    fn a_sink_receives_the_escape_as_one_batch() {
        let (tx, mut rx) = unbounded_channel();
        let clipboard = LocalClipboard::new(Some(tx));

        clipboard.osc52_emit("hello").expect("sink is open");

        assert_eq!(rx.try_recv(), Ok(osc52_sequence("hello")));
        assert!(rx.try_recv().is_err(), "the escape arrived whole");
    }

    /// The channel closes when the UI thread is gone, which is shutdown. The
    /// caller warn-logs and drops the yank rather than writing to a terminal
    /// being restored.
    #[test]
    fn a_closed_sink_reports_the_emit_as_failed() {
        let (tx, rx) = unbounded_channel();
        let clipboard = LocalClipboard::new(Some(tx));
        drop(rx);

        assert!(clipboard.osc52_emit("hello").is_err());
    }
}
