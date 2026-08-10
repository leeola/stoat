use crate::host::ClipboardHost;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    io::{self, Write},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};
use tokio::sync::mpsc::UnboundedSender;

/// Production [`ClipboardHost`] backed by a persistent [`arboard::Clipboard`]
/// on a thread of its own.
///
/// A write to the system clipboard is a round trip to the display server, and
/// every yank makes one. Off the run loop that round trip is time the editor
/// spends waiting on something no one reads back, so writes are queued and the
/// thread makes them.
///
/// The handle cannot just be dropped after each write. On X11 the clipboard
/// contents are served by the owning process only while a handle lives, so a
/// fresh handle per call drops selection ownership the instant it returns and
/// loses the copy unless a clipboard manager races to grab it. Retaining one
/// handle keeps the copy alive, and gives it an owner that outlives any single
/// write. It also avoids arboard's debug-build Drop warning, which prints raw
/// to stderr when a handle drops within 100ms of a write.
///
/// The thread opens nothing until the first command reaches it, which is what
/// preserves fail-late behavior. Machines without a display server (CI,
/// headless servers) surface the failure on the first clipboard use rather
/// than at process startup.
pub struct LocalClipboard {
    commands: Sender<Command>,
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
        let (commands, rx) = mpsc::channel();

        let spawned = thread::Builder::new()
            .name("clipboard".to_owned())
            .spawn(move || serve(&rx, open_arboard));

        // A process that cannot spawn a thread has worse problems than the
        // clipboard, and the receiver dropping here is what turns every later
        // command into the same failure a dead thread gives.
        if let Err(err) = spawned {
            tracing::warn!(
                target: "stoat::host::clipboard",
                error = %err,
                "clipboard thread failed to spawn"
            );
        }

        Self {
            commands,
            osc52_sink,
        }
    }
}

/// One request for the clipboard thread, which owns the handle both need.
enum Command {
    Set(String),
    /// The reply channel a [`ClipboardHost::get`] caller blocks on. A platform
    /// error arrives as [`None`], since the caller treats a failed read and an
    /// empty clipboard the same way.
    Get(Sender<Option<String>>),
}

/// The retained platform handle the clipboard thread serves its commands from.
///
/// Exists to separate the thread's rules from arboard. Retrying a failed write
/// on a fresh handle, and giving up a handle that failed a read, are the parts
/// worth pinning, and no test can reach a display server to pin them against
/// the real thing.
trait ClipboardBackend {
    fn write(&mut self, text: &str) -> io::Result<()>;

    /// The clipboard's text, or [`None`] where the platform reports nothing
    /// this handle can read.
    fn read(&mut self) -> io::Result<Option<String>>;
}

impl ClipboardBackend for arboard::Clipboard {
    fn write(&mut self, text: &str) -> io::Result<()> {
        self.set_text(text).map_err(io::Error::other)
    }

    fn read(&mut self) -> io::Result<Option<String>> {
        match self.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(io::Error::other(err)),
        }
    }
}

fn open_arboard() -> io::Result<arboard::Clipboard> {
    arboard::Clipboard::new().map_err(io::Error::other)
}

/// Serve `commands` from one lazily opened handle until the channel closes.
///
/// Runs on the clipboard thread. `open` is injected so the rules below can be
/// driven without a display server.
fn serve<B: ClipboardBackend>(commands: &Receiver<Command>, open: impl Fn() -> io::Result<B>) {
    let mut handle: Option<B> = None;

    while let Ok(command) = commands.recv() {
        match command {
            Command::Set(text) => {
                if handle
                    .as_mut()
                    .is_some_and(|backend| backend.write(&text).is_ok())
                {
                    continue;
                }

                // Either nothing was open or the open handle failed to write,
                // which is what a display connection gone stale looks like.
                // One fresh handle, one retry.
                handle = match reopen_and_write(&open, &text) {
                    Ok(fresh) => Some(fresh),
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::host::clipboard",
                            error = %err,
                            "clipboard write failed"
                        );
                        None
                    },
                };
            },

            Command::Get(reply) => {
                if handle.is_none() {
                    // Silent where a write is loud. A machine with no display
                    // server reads as an empty clipboard, which is what makes a
                    // paste there a no-op rather than a warning per keystroke.
                    let Ok(fresh) = open() else {
                        let _ = reply.send(None);
                        continue;
                    };
                    handle = Some(fresh);
                }

                let read = handle.as_mut().expect("just opened").read();
                let text = match read {
                    Ok(text) => text,
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::host::clipboard",
                            error = %err,
                            "clipboard read failed"
                        );
                        // A failed read says the same thing about the
                        // connection a failed write does, so the handle goes
                        // with it and the next command opens a new one.
                        handle = None;
                        None
                    },
                };
                let _ = reply.send(text);
            },
        }
    }
}

fn reopen_and_write<B: ClipboardBackend>(
    open: &impl Fn() -> io::Result<B>,
    text: &str,
) -> io::Result<B> {
    let mut fresh = open()?;
    fresh.write(text)?;
    Ok(fresh)
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
    /// Queues the write and returns. The error reports only that the clipboard
    /// thread is gone, never what the display server made of the text, which
    /// is no longer known by the time this returns.
    fn set(&self, text: &str) -> io::Result<()> {
        self.commands
            .send(Command::Set(text.to_owned()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard thread is gone"))
    }

    /// Blocks until the clipboard thread answers, since a paste has nothing to
    /// insert until it does.
    fn get(&self) -> io::Result<Option<String>> {
        let (reply, answer) = mpsc::channel();
        if self.commands.send(Command::Get(reply)).is_err() {
            return Ok(None);
        }
        Ok(answer.recv().unwrap_or(None))
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
    use super::{osc52_sequence, serve, ClipboardBackend, Command, LocalClipboard};
    use crate::host::ClipboardHost;
    use std::{
        io,
        sync::{mpsc, Arc, Mutex, MutexGuard},
    };
    use tokio::sync::mpsc::unbounded_channel;

    /// A display server the test scripts, standing in for the one no test has.
    ///
    /// Cloning hands out another handle onto the same clipboard, which is what
    /// lets an `open` closure produce fresh handles that still agree about what
    /// was copied.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);

    #[derive(Default)]
    struct FakeState {
        text: Option<String>,
        opens: usize,
        /// Counts down, so a scripted failure is spent rather than permanent
        /// and the retry after it can succeed.
        opens_to_fail: usize,
        /// Lets a handle work before it breaks, which is the shape a display
        /// connection going stale under a retained handle actually has.
        writes_before_failing: usize,
        writes_to_fail: usize,
        reads_to_fail: usize,
    }

    impl Fake {
        fn state(&self) -> MutexGuard<'_, FakeState> {
            self.0.lock().expect("fake poisoned")
        }

        /// Run every command through a fresh [`serve`], returning once they are
        /// spent. Pre-loading the channel and dropping the sender is what makes
        /// the thread's loop a synchronous, ordered call.
        fn drive(&self, commands: Vec<Command>) {
            let (tx, rx) = mpsc::channel();
            for command in commands {
                tx.send(command).expect("receiver alive");
            }
            drop(tx);

            serve(&rx, || {
                let mut state = self.state();
                state.opens += 1;
                if state.opens_to_fail > 0 {
                    state.opens_to_fail -= 1;
                    return Err(io::Error::other("no display server"));
                }
                drop(state);
                Ok(self.clone())
            });
        }
    }

    impl ClipboardBackend for Fake {
        fn write(&mut self, text: &str) -> io::Result<()> {
            let mut state = self.state();
            if state.writes_before_failing > 0 {
                state.writes_before_failing -= 1;
            } else if state.writes_to_fail > 0 {
                state.writes_to_fail -= 1;
                return Err(io::Error::other("stale display connection"));
            }
            state.text = Some(text.to_owned());
            Ok(())
        }

        fn read(&mut self) -> io::Result<Option<String>> {
            let mut state = self.state();
            if state.reads_to_fail > 0 {
                state.reads_to_fail -= 1;
                return Err(io::Error::other("stale display connection"));
            }
            Ok(state.text.clone())
        }
    }

    /// A [`Command::Get`] paired with the channel its answer lands on.
    fn get() -> (Command, mpsc::Receiver<Option<String>>) {
        let (reply, answer) = mpsc::channel();
        (Command::Get(reply), answer)
    }

    /// A queued write is still a write the next read has to see, which is what
    /// makes one thread owning the handle worth the arrangement.
    #[test]
    fn a_queued_write_is_visible_to_the_read_behind_it() {
        let fake = Fake::default();
        let (read, answer) = get();

        fake.drive(vec![Command::Set("copied".to_owned()), read]);

        assert_eq!(answer.recv(), Ok(Some("copied".to_owned())));
        assert_eq!(fake.state().opens, 1, "both commands shared one handle");
    }

    /// A retained handle whose display connection has gone stale fails its
    /// write, and nothing before that write says so. Retrying on a fresh handle
    /// is what keeps the yank from being lost to a connection the editor had no
    /// way to know was dead.
    #[test]
    fn a_write_onto_a_stale_handle_retries_on_a_fresh_one() {
        let fake = Fake::default();
        {
            let mut state = fake.state();
            state.writes_before_failing = 1;
            state.writes_to_fail = 1;
        }
        let (read, answer) = get();

        fake.drive(vec![
            Command::Set("first".to_owned()),
            Command::Set("second".to_owned()),
            read,
        ]);

        assert_eq!(answer.recv(), Ok(Some("second".to_owned())));
        assert_eq!(
            fake.state().opens,
            2,
            "the failed write opened a new handle"
        );
    }

    /// The retry is one deep. A second failure drops the handle rather than
    /// looping, and the thread stays up for whatever comes next.
    #[test]
    fn a_write_that_fails_twice_gives_up_without_stopping_the_thread() {
        let fake = Fake::default();
        fake.state().writes_to_fail = 2;
        let (read, answer) = get();

        fake.drive(vec![Command::Set("lost".to_owned()), read]);

        assert_eq!(answer.recv(), Ok(None), "nothing was written");
        assert_eq!(fake.state().opens, 2, "one retry, not a loop");
    }

    /// A read failing says the same thing about the connection a write failing
    /// does, so the handle it failed on is not the one the next read uses. The
    /// text is already on the clipboard, which is how the reopened handle shows
    /// it works rather than merely existing.
    #[test]
    fn a_failed_read_gives_up_its_handle() {
        let fake = Fake::default();
        fake.state().reads_to_fail = 1;
        let (first, first_answer) = get();
        let (second, second_answer) = get();

        fake.drive(vec![Command::Set("copied".to_owned()), first, second]);

        assert_eq!(first_answer.recv(), Ok(None), "the failed read reads empty");
        assert_eq!(second_answer.recv(), Ok(Some("copied".to_owned())));
        assert_eq!(fake.state().opens, 2, "the second read opened a new handle");
    }

    /// A machine with no display server pastes nothing rather than erroring, so
    /// the caller no-ops instead of warning on every paste.
    #[test]
    fn no_display_server_reads_as_an_empty_clipboard() {
        let fake = Fake::default();
        fake.state().opens_to_fail = 1;
        let (read, answer) = get();

        fake.drive(vec![read]);

        assert_eq!(answer.recv(), Ok(None));
    }

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
