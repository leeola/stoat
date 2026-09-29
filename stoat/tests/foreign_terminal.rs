//! Guard tests for the foreign-terminal path, driving a real stoat process on a
//! real pty with nothing answering its ident handshake.
//!
//! Gated on the `fixture` feature, so a plain `cargo test` never builds them.
//! They also need the binary built, which testing the library does not do:
//!
//! ```sh
//! cargo build -p stoat_bin
//! cargo test -p stoat --features fixture --test foreign_terminal
//! ```
//!
//! Every layer these assert over has its own unit tests. Only this tier shows
//! the layers composing in a real process. A session that never hears from a
//! stoatty must put nothing but its detection probe on the wire, however the
//! handshake, the emit gate, and the render branches each behave in isolation.
//! And input after a resize depends on the real binary, which blocks SIGWINCH
//! on every thread and takes it on one. No unit test controls that.

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use stoat::term_screen::TermScreen;
use tempfile::TempDir;

/// The APC introducer plus the namespace tag every stoatty frame opens with.
const APC_INTRODUCER: &[u8] = b"\x1b_Gstoatty;";

/// Pause after each key, long enough for a frame to be painted and flushed.
const KEY_SETTLE: Duration = Duration::from_millis(600);

/// How long to wait for the process to exit after the quit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the screen has to show what a step expects. Far past one frame and
/// the startup handshake's fallback window, which the first step waits out, so
/// a loaded machine still passes, and short enough that a missed step fails the
/// run quickly.
const SCREEN_TIMEOUT: Duration = Duration::from_secs(10);

/// A stoat process on a real pty, with everything it writes collected.
///
/// The two directories are held for the run, so the fixture repository and the
/// scratch home outlive the process that reads them.
struct Session {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    collector: JoinHandle<()>,
    dirs: (TempDir, TempDir),
    /// Each grid the pty took, as the output length when it took effect, then
    /// its columns and rows. The first entry is the grid the session started
    /// on.
    grids: Vec<(usize, u16, u16)>,
}

impl Session {
    /// Spawn a session with [`Self::spawn`], and wait until it reads keys.
    ///
    /// The startup handshake holds fd 0 until it gives up on a reply, which
    /// nothing here sends, and it hands over what was typed meanwhile only as it
    /// ends. So an insert-mode round trip on screen shows the handshake is over
    /// and the input thread reads keys, however slowly the process started.
    fn start(cols: u16, rows: u16) -> Self {
        let mut session = Session::spawn(cols, rows);
        session.send("i");
        let inserting = session.settles(|screen| screen.contains(" INS "));
        session.send("\x1b");
        let normal = session.settles(|screen| screen.contains(" NOR "));

        assert_eq!(
            (inserting, normal),
            (true, true),
            "the session reads keys:\n{}",
            session.screen_text(),
        );
        session
    }

    /// Spawn stoat in a `history` fixture repository on a `cols` by `rows` pty.
    fn spawn(cols: u16, rows: u16) -> Self {
        let binary = stoat_binary();
        let (repo_dir, home_dir, root, home) = fixture_workspace();
        let pair = portable_pty::native_pty_system()
            .openpty(pty_size(cols, rows))
            .expect("open a pty");

        let mut cmd = CommandBuilder::new(&binary);
        cmd.cwd(&root);
        // Whatever launched the test may itself be running under stoatty. Every
        // marker of that has to go, or the child inherits a claim no one will
        // honor. The XDG overrides go too, so config, data, and state resolve
        // under the scratch home.
        for key in [
            "STOATTY",
            "STOATTY_VERSION",
            "STOATTY_LOG_ID",
            "STOATTY_WINDOW_SOCKET",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", &home);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).expect("spawn stoat");
        // The child owns the only slave fd from here, so the reader below sees
        // EOF when it exits rather than blocking forever on this one.
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("pty writer");
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let collector = thread::spawn({
            let output = Arc::clone(&output);
            move || {
                let mut chunk = [0u8; 8192];
                while let Ok(read) = reader.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    output
                        .lock()
                        .expect("output lock")
                        .extend_from_slice(&chunk[..read]);
                }
            }
        });

        Session {
            child,
            master: pair.master,
            writer,
            output,
            collector,
            dirs: (repo_dir, home_dir),
            grids: vec![(0, cols, rows)],
        }
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("write keys");
        self.writer.flush().expect("flush keys");
    }

    /// Resize the pty, which has the kernel send the session a SIGWINCH.
    fn resize(&mut self, cols: u16, rows: u16) {
        let at = self.output.lock().expect("output lock").len();
        self.master
            .resize(pty_size(cols, rows))
            .expect("resize the pty");
        self.grids.push((at, cols, rows));
    }

    /// Whether the session writes `bytes` within [`SCREEN_TIMEOUT`], for what
    /// never reaches the screen, such as the handshake's probe.
    fn wrote(&self, bytes: &[u8]) -> bool {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            if find(&self.output.lock().expect("output lock"), bytes).is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether the screen reaches a state `done` accepts within
    /// [`SCREEN_TIMEOUT`].
    ///
    /// Read off the screen rather than the byte stream, because a frame writes
    /// only the cells that changed. A report over an earlier one in the same
    /// style reaches the wire in pieces wherever the two share a character.
    ///
    /// Polled rather than slept, so a passing run waits one frame rather than
    /// the whole bound, and a stalled session still gets the whole bound.
    fn settles(&self, done: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            if done(&self.screen_text()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The screen's rows as text, one line each, for a search or a failure
    /// message.
    fn screen_text(&self) -> String {
        let screen = self.screen();
        (0..screen.rows())
            .map(|row| screen.row(row).iter().map(|cell| cell.ch).collect())
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// The screen the output so far paints, replayed onto a grid that takes
    /// each resize at the point in the output where it happened.
    fn screen(&self) -> TermScreen {
        let output = self.output.lock().expect("output lock");
        let (_, cols, rows) = self.grids[0];
        let mut screen = TermScreen::new(rows, cols);

        let mut fed = 0;
        for &(at, cols, rows) in &self.grids[1..] {
            screen.feed(&output[fed..at]);
            screen.resize(rows, cols);
            fed = at;
        }
        screen.feed(&output[fed..]);
        screen
    }

    /// Quit the session, and return how it exited with everything it wrote.
    ///
    /// Through the command rather than ctrl-c, which quits nothing here. Normal
    /// mode binds that key to a comment toggle and a run pane to an interrupt,
    /// and no signal handler stands behind either. Nothing is modified in a
    /// fresh fixture workspace, so this quits with no confirmation to answer.
    fn quit(mut self) -> (ExitStatus, Vec<u8>) {
        self.send(":quit-all\r");
        let Session {
            mut child,
            master: _master,
            writer,
            output,
            collector,
            dirs: _dirs,
            grids: _,
        } = self;
        drop(writer);

        let status = wait_for_exit(&mut child);
        collector.join().expect("join the reader");
        let captured = output.lock().expect("output lock").clone();
        (status, captured)
    }
}

/// The commit picker is where the leak was reported, so this drives the surface
/// that produced it rather than an idle screen. The picker's list and diff both
/// pool, and its graph strokes paths, so a regression in any of the three gates
/// shows up here as a frame that should not exist.
#[test]
fn a_foreign_terminal_receives_nothing_but_the_hello_probe() {
    // The oldest commit of the history fixture, which the picker lists.
    let listed = "add api endpoints";

    let mut session = Session::start(120, 40);
    session.send(":git-ls\r");
    let opened = session.settles(|screen| screen.contains(listed));
    session.send("\x1b[B");
    thread::sleep(KEY_SETTLE);
    session.send("\x1b");
    // A quit sent while the Escape is still unread reaches the input thread
    // with it in one read, which parses as an Alt chord, so the quit waits for
    // the picker to close.
    let closed = session.settles(|screen| !screen.contains(listed));
    let (status, captured) = session.quit();

    assert_eq!(
        (opened, closed),
        (true, true),
        "the picker opens, and the Escape closes it"
    );
    assert_eq!(
        apc_subcommands(&captured),
        ["hello"],
        "the probe is the only frame a terminal that cannot read them should \
         ever see, but the session emitted more",
    );
    assert!(
        status.success(),
        "the session should quit cleanly when asked, got {status:?}",
    );
}

/// A resize repaints the screen on its own, and a key typed after it reaches the
/// app on its own, each with no later key to carry it.
///
/// The process blocks SIGWINCH on every thread, and one thread takes it and
/// writes the input thread's resize pipe. crossterm never sees the signal, so a
/// key that turns ready with a resize reads like any other. The cursor position
/// at the right end of the bar shows the repaint, and the follow toggle's
/// report shows the key.
#[test]
fn a_resize_and_the_key_after_it_land_without_another_key() {
    let mut session = Session::start(120, 40);
    session.resize(100, 40);
    let repainted = session.settles(bar_reaches_the_edge);
    let after_resize = session.screen_text();

    session.send(" Gf");
    let reported = session.settles(|screen| screen.contains("follow changes on"));
    let after_key = session.screen_text();
    let (status, _) = session.quit();

    assert!(
        repainted,
        "the resize repaints the bar at the new width:\n{after_resize}"
    );
    assert!(
        reported,
        "the follow report reaches the screen with no later key:\n{after_key}"
    );
    assert!(
        status.success(),
        "the session quits cleanly when asked, got {status:?}",
    );
}

/// A resize that lands while the startup handshake owns fd 0 repaints the
/// screen once the session starts reading.
///
/// A tiling window manager resizes a new terminal as it opens, so the first
/// resize often lands inside the handshake. The thread that takes SIGWINCH
/// writes the input thread's pipe during the handshake too, and the byte waits
/// there until the session starts reading.
#[test]
fn a_resize_during_the_handshake_repaints_after_it() {
    let mut session = Session::spawn(120, 40);
    // The probe goes out after the resize pipe is registered, and nothing here
    // answers it, so the handshake holds fd 0 for its whole fallback window.
    let probing = session.wrote(APC_INTRODUCER);
    session.resize(100, 40);
    let repainted = session.settles(bar_reaches_the_edge);
    let screen = session.screen_text();
    let (status, _) = session.quit();

    assert_eq!(
        (probing, repainted),
        (true, true),
        "the resize repaints the bar at the new width:\n{screen}"
    );
    assert!(
        status.success(),
        "the session quits cleanly when asked, got {status:?}",
    );
}

/// Whether the status bar on `screen` reaches the right edge of the grid.
///
/// The bar ends with the cursor position, right-aligned to the width stoat laid
/// out against. A frame laid out for a wider grid has that end cut off, so the
/// position shows only once stoat lays out against the grid the pty has.
fn bar_reaches_the_edge(screen: &str) -> bool {
    screen
        .lines()
        .last()
        .is_some_and(|bar| bar.trim_end().ends_with("1:1"))
}

/// The stoat binary, resolved from this test executable's own directory.
///
/// `CARGO_BIN_EXE_*` is unavailable here, since the binary belongs to
/// `stoat_bin` rather than this package. Walking up from the test binary
/// instead follows whatever target directory and profile the run is using.
fn stoat_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("this test's own path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let binary = path.join("stoat");

    assert!(
        binary.is_file(),
        "no stoat binary at {}. Testing the library does not build it, so run \
         `cargo build -p stoat_bin` first. This opt-in tier fails loudly rather \
         than skipping when a prerequisite is missing.",
        binary.display(),
    );
    binary
}

/// A `history` fixture repository plus a scratch home, so the spawned session
/// reads no config and writes no state belonging to whoever ran the test.
///
/// Both [`TempDir`] guards come back so the caller can hold them for the run.
fn fixture_workspace() -> (TempDir, TempDir, PathBuf, PathBuf) {
    let repo_dir = tempfile::tempdir().expect("create the repo tempdir");
    let home_dir = tempfile::tempdir().expect("create the home tempdir");
    let root = std::fs::canonicalize(repo_dir.path()).expect("canonicalize the repo dir");
    let home = std::fs::canonicalize(home_dir.path()).expect("canonicalize the home dir");

    stoat::fixture::materialize("history", &root).expect("materialize the history fixture");
    (repo_dir, home_dir, root, home)
}

/// Every stoatty sub-command in `bytes`, in emission order.
///
/// Reads the name between the introducer and whatever ends it -- an argument
/// separator, the string terminator, or the bell some intermediaries substitute
/// for it -- so a frame counts whether or not it carries arguments.
fn apc_subcommands(bytes: &[u8]) -> Vec<String> {
    let mut subs = Vec::new();
    let mut rest = bytes;

    while let Some(at) = find(rest, APC_INTRODUCER) {
        let payload = &rest[at + APC_INTRODUCER.len()..];
        let end = payload
            .iter()
            .position(|&b| b == b';' || b == 0x1b || b == 0x07)
            .unwrap_or(payload.len());
        subs.push(String::from_utf8_lossy(&payload[..end]).into_owned());
        rest = &payload[end..];
    }

    subs
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Poll until the child exits, killing it and failing if it overruns
/// [`EXIT_TIMEOUT`]. A hung session is a failure of the same run, not something
/// to leave behind for the next one.
fn wait_for_exit(child: &mut Box<dyn Child + Send + Sync>) -> ExitStatus {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stoat did not exit within {EXIT_TIMEOUT:?} of the quit");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
