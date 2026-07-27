//! Guard test for the foreign-terminal path, driving a real stoat process on a
//! real pty with nothing answering its ident handshake.
//!
//! Gated on the `fixture` feature, so a plain `cargo test` never builds it. It
//! also needs the binary built, which testing the library does not do:
//!
//! ```sh
//! cargo build -p stoat_bin
//! cargo test -p stoat --features fixture --test foreign_terminal
//! ```
//!
//! Every layer this asserts over has its own unit tests. What only this tier
//! can show is the three of them composing. A session that never hears from a
//! stoatty must put nothing but its detection probe on the wire, however the
//! handshake, the emit gate, and the render branches each behave in isolation.

use portable_pty::{CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tempfile::TempDir;

/// The APC introducer plus the namespace tag every stoatty frame opens with.
const APC_INTRODUCER: &[u8] = b"\x1b_Gstoatty;";

/// Long enough to cover process startup plus the handshake's own reply window,
/// which nothing here answers, so the session settles as foreign before the
/// first key arrives.
const SETTLE: Duration = Duration::from_millis(1500);

/// Pause after each key, long enough for a frame to be painted and flushed.
const KEY_SETTLE: Duration = Duration::from_millis(600);

/// How long to wait for the process to exit after Ctrl-c.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The commit picker is where the leak was reported, so this drives the surface
/// that produced it rather than an idle screen. The picker's list and diff both
/// pool, and its graph strokes paths, so a regression in any of the three gates
/// shows up here as a frame that should not exist.
#[test]
fn a_foreign_terminal_receives_nothing_but_the_hello_probe() {
    let binary = stoat_binary();
    let (_repo_dir, _home_dir, root, home) = fixture_workspace();

    let pty = portable_pty::native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");

    let mut cmd = CommandBuilder::new(&binary);
    cmd.cwd(&root);
    // Whatever launched the test may itself be running under stoatty. Every
    // marker of that has to go, or the child inherits a claim no one will honor.
    for key in [
        "STOATTY",
        "STOATTY_VERSION",
        "STOATTY_LOG_ID",
        "STOATTY_WINDOW_SOCKET",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("HOME", &home);
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn stoat");
    // The child owns the only slave fd from here, so the reader below sees EOF
    // when it exits rather than blocking forever on this one.
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("pty writer");
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

    thread::sleep(SETTLE);
    for keys in [":git-ls\r", "\x1b[B", "\x1b"] {
        writer.write_all(keys.as_bytes()).expect("write keys");
        writer.flush().expect("flush keys");
        thread::sleep(KEY_SETTLE);
    }

    writer.write_all(b"\x03").expect("write ctrl-c");
    writer.flush().expect("flush ctrl-c");
    drop(writer);

    let status = wait_for_exit(&mut child);
    collector.join().expect("join the reader");
    let captured = output.lock().expect("output lock").clone();

    assert_eq!(
        apc_subcommands(&captured),
        ["hello"],
        "the probe is the only frame a terminal that cannot read them should \
         ever see, but the session emitted more",
    );
    assert!(
        status.success(),
        "the session should quit cleanly on ctrl-c, got {status:?}",
    );
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
fn wait_for_exit(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> portable_pty::ExitStatus {
    let deadline = std::time::Instant::now() + EXIT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stoat did not exit within {EXIT_TIMEOUT:?} of ctrl-c");
        }
        thread::sleep(Duration::from_millis(50));
    }
}
