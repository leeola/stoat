//! Answering "am I running inside stoatty", which a program has to know before
//! it emits anything richer than plain VT.
//!
//! Most of the protocol degrades to nothing in a foreign terminal, since an APC
//! string is consumed and ignored. Streamed content does not: a popover's text
//! and a page fill's cells travel outside the frame wrapper and print as
//! characters on a terminal that never opened the capture. So a program has to
//! establish who it is talking to before sending any of it.
//!
//! There are two answers, and they are not equally good. The environment says
//! what launched the process, which is cheap and wrong the moment another
//! terminal is nested inside stoatty. The handshake asks the terminal itself and
//! is definitive, but costs a round trip and sole ownership of stdin while it
//! runs. Prefer [`handshake`], and treat [`env_says_stoatty`] as the hint it is.

use crate::{
    command::{self, HelloCommand, IdentReply},
    frame,
};
use std::{
    io::{self, IsTerminal, Write},
    time::Duration,
};

/// The variable stoatty sets in every process it spawns.
pub const STOATTY_ENV: &str = "STOATTY";

/// The variable carrying stoatty's own release, set beside [`STOATTY_ENV`].
///
/// Names the terminal build, not what it can render. Gate features on the
/// protocol version an [`IdentReply`] carries instead.
pub const STOATTY_VERSION_ENV: &str = "STOATTY_VERSION";

/// How long [`handshake`] waits before giving up on a terminal that answers
/// neither query it sends.
///
/// A fallback rather than a budget for the round trip. The cursor-position
/// report is what ends the wait on a foreign terminal and the ident reply is
/// what ends it on a stoatty, so link latency never decides the verdict and this
/// only has to be longer than any terminal takes to answer at all.
pub const HANDSHAKE_FALLBACK: Duration = Duration::from_secs(2);

/// Whether the environment claims this process was launched by stoatty.
///
/// A hint, not an answer. The variable is inherited by everything downstream, so
/// a different terminal running inside a stoatty session reports true while
/// understanding none of the protocol. A [`handshake`] result always overrides
/// this, and is what a program should reach for when it can afford the round
/// trip.
///
/// It earns its keep where the handshake cannot run at all. That covers the
/// window before stdin can be owned, a child process with no way to probe, and
/// any fast path where being wrong only costs a plainer frame.
pub fn env_says_stoatty() -> bool {
    says_stoatty(std::env::var(STOATTY_ENV).ok().as_deref())
}

/// The [`env_says_stoatty`] rule with the variable supplied, so the precedence
/// is testable without touching the process environment.
fn says_stoatty(var: Option<&str>) -> bool {
    var == Some("1")
}

/// Announce this program to the terminal with `hello` and read back who
/// answered.
///
/// Writes the hello frame followed by a cursor-position query, then reads raw
/// stdin until one of them is answered. A stoatty answers both through one
/// queue, in that order. Every other terminal answers only the second. So a
/// report arriving with no ident ahead of it settles the question however slow
/// the link, rather than a timeout guessing at it.
///
/// The hello goes out unconditionally, since an APC frame degrades to nothing in
/// a foreign terminal. A stdin that is not a tty cannot carry an answer back at
/// all, so that case reports `None` without probing or waiting.
///
/// Returns the terminal's reply when a stoatty answered, plus every byte read
/// that was neither answer. Those are what someone typed while this owned fd 0,
/// and a caller that reads input should replay them rather than lose what was
/// typed at launch.
///
/// Call this before handing stdin to a line editor or event reader. Those parse
/// what they read, and none of them can surface an APC reply, which is why this
/// reads fd 0 directly and has to go first.
pub fn handshake(hello: &HelloCommand, fallback: Duration) -> (Option<IdentReply>, Vec<u8>) {
    let hello = command::encode_hello(hello);
    let probing = stdin_is_tty();

    {
        // The query rides the same flush as the hello, so the terminal queues
        // both answers together and the order between them is its own. Sent only
        // when something can answer, since an unread report would sit in the
        // terminal's input for whatever runs next.
        let mut stdout = io::stdout().lock();
        let wrote = match probing {
            true => stdout
                .write_all(&hello)
                .and_then(|()| stdout.write_all(b"\x1b[6n")),
            false => stdout.write_all(&hello),
        };
        if wrote.is_err() || stdout.flush().is_err() {
            return (None, Vec::new());
        }
    }

    match probing {
        true => read_ident_reply(fallback),
        false => (None, Vec::new()),
    }
}

/// What the bytes read so far say about the terminal on the other end.
#[derive(Debug, PartialEq, Eq)]
enum Handshake {
    /// A stoatty answered with its ident.
    Stoatty(IdentReply),
    /// The cursor-position query came back with no ident ahead of it, which only
    /// a terminal that ignored the hello does.
    Foreign,
    /// Neither answer has arrived in full yet.
    Pending,
}

/// Read the handshake's answer out of `buf`, removing the bytes it accounted for
/// and leaving the rest.
///
/// What remains is what someone typed while the probe was running, which the
/// caller replays. Both answers are taken out so none of a terminal's own
/// chatter is replayed as input.
///
/// An APC frame that is not an ident reply is consumed and ignored, since it is
/// some other terminal-to-program message and not the answer being waited on.
fn scan_handshake(buf: &mut Vec<u8>) -> Handshake {
    if let Some(span) = frame::apc_span(buf) {
        let reply =
            frame::decode(&buf[span.clone()]).and_then(|frame| command::decode_ident_reply(&frame));
        buf.drain(span);
        if let Some(reply) = reply {
            // A stoatty queues the report behind the ident, so it is usually
            // already here. Taking it now keeps it out of the replay.
            if let Some(span) = cpr_span(buf) {
                buf.drain(span);
            }
            return Handshake::Stoatty(reply);
        }
    }

    match cpr_span(buf) {
        Some(span) => {
            buf.drain(span);
            Handshake::Foreign
        },
        None => Handshake::Pending,
    }
}

/// The span of a cursor-position report in `bytes`, `ESC [ row ; col R`.
///
/// Scans past any other CSI sequence, since a keystroke typed during the probe
/// arrives as one and must not be mistaken for the answer.
fn cpr_span(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut from = 0;
    while let Some(offset) = bytes[from..].windows(2).position(|pair| pair == b"\x1b[") {
        let start = from + offset;
        let params_at = start + 2;
        let Some(end) = bytes[params_at..].iter().position(|byte| *byte == b'R') else {
            // No terminator yet, and any later `ESC [` would be inside this
            // unfinished sequence rather than a report of its own.
            return None;
        };

        let params = &bytes[params_at..params_at + end];
        let (row, col) = params.split_at(params.iter().position(|byte| *byte == b';')?);
        let digits = |field: &[u8]| !field.is_empty() && field.iter().all(u8::is_ascii_digit);
        if digits(row) && digits(&col[1..]) {
            return Some(start..params_at + end + 1);
        }
        from = start + 2;
    }
    None
}

/// Whether fd 0 is a terminal, and so whether anything can answer the probe.
#[cfg(unix)]
fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

#[cfg(not(unix))]
fn stdin_is_tty() -> bool {
    false
}

/// Read raw stdin until the terminal answers one of the handshake's two queries,
/// or `fallback` elapses, returning the ident reply if a stoatty was the one
/// that answered.
///
/// The wait ends on an answer rather than on the clock, so a slow link delays
/// startup by its own round trip instead of being misread as a foreign terminal.
/// The elapsed case is a terminal that answered neither.
#[cfg(unix)]
fn read_ident_reply(fallback: Duration) -> (Option<IdentReply>, Vec<u8>) {
    let deadline = std::time::Instant::now() + fallback;
    let mut buf = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut fds = [libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        }];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) } <= 0 {
            break;
        }

        let mut chunk = [0u8; 512];
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);

        match scan_handshake(&mut buf) {
            Handshake::Stoatty(reply) => return (Some(reply), buf),
            Handshake::Foreign => return (None, buf),
            Handshake::Pending => {},
        }
    }

    (None, buf)
}

#[cfg(not(unix))]
fn read_ident_reply(_fallback: Duration) -> (Option<IdentReply>, Vec<u8>) {
    (None, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::{cpr_span, says_stoatty, scan_handshake, Handshake};
    use crate::command::{encode_ident_reply, IdentReply};

    fn ident() -> IdentReply {
        IdentReply {
            pid: 7,
            log_id: "abc".into(),
            hostname: "host".into(),
            version: "1.2.3".into(),
            protocol: crate::PROTOCOL_VERSION,
        }
    }

    /// The variable is inherited by everything downstream, so only its exact
    /// value means anything and everything else is somebody else's terminal.
    #[test]
    fn only_the_set_marker_reads_as_stoatty() {
        assert!(says_stoatty(Some("1")));
        assert!(!says_stoatty(None), "unset is a foreign terminal");
        assert!(!says_stoatty(Some("0")), "explicitly off");
        assert!(!says_stoatty(Some("")), "cleared by an intermediate");
        assert!(!says_stoatty(Some("true")), "not the marker stoatty writes");
    }

    /// A stoatty queues its ident ahead of the cursor-position report, so a
    /// report with no ident before it means no stoatty, however slow the link.
    #[test]
    fn the_probe_reads_a_verdict_out_of_what_arrived() {
        let cpr = b"\x1b[24;80R".to_vec();
        let ident_frame = encode_ident_reply(&ident());
        let cases: Vec<(&str, Vec<u8>, Handshake, &[u8])> = vec![
            (
                "a stoatty answers the ident first, then the report",
                [ident_frame.clone(), cpr.clone()].concat(),
                Handshake::Stoatty(ident()),
                b"",
            ),
            (
                "a foreign terminal answers only the report",
                cpr.clone(),
                Handshake::Foreign,
                b"",
            ),
            (
                "neither answer yet, so nothing is decided or consumed",
                b"hi".to_vec(),
                Handshake::Pending,
                b"hi",
            ),
            (
                "typing around a stoatty's answers is what is left over",
                [b"ab".to_vec(), ident_frame, b"cd".to_vec(), cpr.clone()].concat(),
                Handshake::Stoatty(ident()),
                b"abcd",
            ),
            (
                "and typing around a foreign terminal's report likewise",
                [b"ab".to_vec(), cpr.clone(), b"cd".to_vec()].concat(),
                Handshake::Foreign,
                b"abcd",
            ),
            (
                "an arrow key is a CSI too, and is not the report",
                b"\x1b[A".to_vec(),
                Handshake::Pending,
                b"\x1b[A",
            ),
            (
                "an arrow key ahead of the report does not hide it",
                [b"\x1b[A".to_vec(), cpr].concat(),
                Handshake::Foreign,
                b"\x1b[A",
            ),
            (
                "a half-arrived report decides nothing",
                b"\x1b[24;8".to_vec(),
                Handshake::Pending,
                b"\x1b[24;8",
            ),
        ];

        for (name, bytes, verdict, leftover) in cases {
            let mut buf = bytes;
            assert_eq!(scan_handshake(&mut buf), verdict, "{name}");
            assert_eq!(buf, leftover, "leftover for: {name}");
        }
    }

    /// A report is only an answer once its terminator has arrived, and any other
    /// CSI in the way is somebody typing.
    #[test]
    fn a_cursor_report_is_found_past_other_sequences() {
        assert_eq!(cpr_span(b"\x1b[24;80R"), Some(0..8));
        assert_eq!(cpr_span(b"\x1b[A\x1b[24;80R"), Some(3..11));
        assert_eq!(cpr_span(b"\x1b[24;80"), None, "no terminator yet");
        assert_eq!(cpr_span(b"hello"), None, "no sequence at all");
    }
}
