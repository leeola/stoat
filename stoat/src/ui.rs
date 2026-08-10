//! Terminal IO, on two threads of its own so neither direction can stall the
//! other or the main thread.
//!
//! The UI thread owns the terminal. It runs a single-threaded tokio runtime,
//! sets the terminal up and restores it, and flushes rendered frames and
//! stoatty APC batches to stdout.
//!
//! A second thread does nothing but read stdin and forward what it finds. It
//! exists because every flush above is a blocking write, and a stdout that
//! stops draining parks the thread doing it. Keeping the two apart is what
//! makes typing survive a saturated link, where a multi-MB batch can take
//! seconds to go out.
//!
//! Neither thread does any of the editor's work, so terminal IO latency is
//! independent of main-thread workload as well.

use crate::vt_input;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    },
    execute, queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::buffer::Buffer;
use std::{
    backtrace::Backtrace,
    io::{self, Write},
    ops::Range,
    panic,
    sync::{Arc, Once},
    thread,
    time::{Duration, Instant},
};
use stoatty_protocol::{
    command::{self, HelloCommand, IdentReply},
    frame,
};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    watch,
};

/// One rendered frame published from the main thread to the UI thread.
///
/// Carries the painted grid plus the optional terminal-cursor cell. `cursor`
/// is `Some((col, row))` only when running inside stoatty and the focused
/// document editor delegates its primary cursor to the terminal cursor;
/// otherwise it is `None` and the cursor stays hidden, with the editor
/// painting its own cursor cell into `buffer`.
pub struct RenderFrame {
    pub buffer: Arc<Buffer>,
    pub cursor: Option<(u16, u16)>,
    /// Raw VT that re-stamps diagnostic curly underlines over `buffer` after it
    /// is drawn, empty outside stoatty or when no diagnostic span is visible.
    /// Written to stdout right after the grid draw so it decorates the exact
    /// frame it was built for. See [`crate::render::undercurl`].
    pub undercurl: Vec<u8>,
    /// When the frame's first event arrived, for measuring input-to-flush
    /// latency on the UI thread. `Some` only for input-driven frames; `None`
    /// for redraw-notify and PTY wakes, which carry no input to time.
    ///
    /// The render watch is latest-wins, so a frame superseded before the UI
    /// thread draws it is never measured. The recorded distribution therefore
    /// covers frames actually flushed, which is the user-visible latency.
    #[cfg(feature = "perf")]
    pub input_time: Option<Instant>,
}

/// Install a process-global panic hook that restores the terminal before the
/// default hook runs, so a panic in either the main thread or the UI thread
/// leaves cooked mode + the main screen + the panic message visible to the
/// user. Logs `panic_message`, `location`, and a captured backtrace via
/// [`tracing::error`] so the same information is preserved in
/// `stoat-<pid>.log` after the terminal scrollback is gone. Idempotent across
/// repeated calls.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let prior = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            ratatui::restore();

            let panic_message = match info.payload().downcast_ref::<&'static str>() {
                Some(message) => *message,
                None => match info.payload().downcast_ref::<String>() {
                    Some(message) => message.as_str(),
                    None => "Box<Any>",
                },
            };
            let location = info
                .location()
                .map(|loc| format!("{}:{}", loc.file(), loc.line()));
            let backtrace = Backtrace::force_capture();
            tracing::error!(panic = true, ?location, %panic_message, %backtrace, "stoat panic");

            prior(info);
        }));
    });
}

/// `stoatty_tx` carries the ident handshake's one-shot answer to the app. The
/// app cannot learn it any other way, since the handshake needs raw mode and
/// sole ownership of fd 0, both of which live on this thread.
pub fn spawn(
    event_tx: UnboundedSender<Event>,
    mut render_rx: watch::Receiver<Option<RenderFrame>>,
    mut apc_rx: UnboundedReceiver<Vec<u8>>,
    stoatty_tx: UnboundedSender<bool>,
    mouse_captured: bool,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        rt.block_on(async move {
            let mut terminal = ratatui::init();
            // Without this a paste arrives as its characters, which normal mode
            // would run as commands. Bracketed, it arrives whole as one event.
            execute!(io::stdout(), EnableBracketedPaste)?;
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            }
            let result = run(
                &event_tx,
                &mut render_rx,
                &mut apc_rx,
                &stoatty_tx,
                &mut terminal,
            )
            .await;
            if mouse_captured {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            ratatui::restore();
            result
        })
    })
}

async fn run(
    event_tx: &UnboundedSender<Event>,
    render_rx: &mut watch::Receiver<Option<RenderFrame>>,
    apc_rx: &mut UnboundedReceiver<Vec<u8>>,
    stoatty_tx: &UnboundedSender<bool>,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    // Main thread needs terminal dimensions before it can render the first frame
    let size = terminal.size()?;
    if event_tx
        .send(Event::Resize(size.width, size.height))
        .is_err()
    {
        return Ok(());
    }

    // The app renders its first frames while this is still waiting for a reply,
    // so they go out through the foreign-terminal fallback and the report is
    // what upgrades the session once one arrives.
    let (stoatty, typed) = stoatty_handshake();
    let _ = stoatty_tx.send(stoatty);

    // What was typed while the handshake owned fd 0, replayed before the input
    // thread starts so it arrives ahead of anything typed after. Decoding is
    // best-effort, and the alternative it replaces was dropping all of it.
    for event in vt_input::decode(&typed) {
        if event_tx.send(event).is_err() {
            return Ok(());
        }
    }

    // Only now, because the handshake reads raw fd 0 and nothing else may be
    // reading stdin while it does.
    //
    // On its own thread because every arm of the loop below is a blocking write
    // to stdout, and a full pipe parks the whole loop until it drains. Reading
    // stdin from there means a saturated link stops keystrokes being read at
    // all, which is the one thing this thread exists to prevent.
    thread::spawn({
        let event_tx = event_tx.clone();
        move || forward_input(&event_tx, crossterm::event::read)
    });

    // UI-thread-local input-to-flush latency, logged periodically. The main
    // thread keeps its own PerfStats, so this needs no cross-thread channel.
    #[cfg(feature = "perf")]
    let mut ui_perf = crate::perf::PerfStats::default();
    #[cfg(feature = "perf")]
    let mut recorded_frames: usize = 0;

    loop {
        // Biased toward the frame, because the render arm drains the APC queue
        // itself once the grid is drawn. Taking a batch first would write pool
        // content that the grid then paints over, which is the wrong way round.
        tokio::select! {
            biased;

            changed = render_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                // Open a synchronized update so the cell diff, undercurl re-stamp,
                // and APC batch below commit as one frame. Queued rather than
                // flushed, so ratatui's draw flush carries it ahead of the cell
                // diff in the same write.
                queue!(io::stdout(), BeginSynchronizedUpdate)?;
                // Clone the latest frame's Arc out of the watch and drop the
                // borrow before drawing, so the slow terminal flush never holds
                // the lock the render thread needs to publish the next frame.
                // Cloning the Arc is a refcount bump, not a grid copy.
                #[cfg(feature = "perf")]
                let mut input_time = None;
                let framed = {
                    let latest = render_rx.borrow_and_update();
                    latest.as_ref().map(|src| {
                        #[cfg(feature = "perf")]
                        {
                            input_time = src.input_time;
                        }
                        (src.buffer.clone(), src.cursor, src.undercurl.clone())
                    })
                };
                let undercurl = match framed {
                    Some((buffer, cursor, undercurl)) => {
                        terminal.draw(|f| {
                            let dst = f.buffer_mut();
                            if dst.area == buffer.area {
                                dst.content.clone_from(&buffer.content);
                            } else {
                                copy_clamped(dst, &buffer);
                            }
                            if let Some((col, row)) = cursor {
                                f.set_cursor_position((col, row));
                            }
                        })?;
                        // Drop the Arc before draining APC so the main thread's
                        // recycle can reclaim this buffer's allocation via
                        // Arc::try_unwrap instead of falling back to a fresh one.
                        drop(buffer);
                        undercurl
                    },
                    None => Vec::new(),
                };
                // Re-stamp diagnostic curly underlines over the grid just drawn,
                // before the APC batches composite over the same stdout.
                if !undercurl.is_empty() {
                    let mut stdout = io::stdout();
                    stdout.write_all(&undercurl)?;
                    stdout.flush()?;
                }
                // Write any stoatty APC byte batches the app pushed for this
                // frame to the same stdout, after the grid frame so the pool
                // composites over the content just drawn.
                drain_apc(apc_rx)?;
                // Close the synchronized update so the cell diff, undercurl, and
                // APC batch commit as one atomic frame.
                execute!(io::stdout(), EndSynchronizedUpdate)?;
                // The frame's bytes are out, so stop the input-to-flush clock.
                #[cfg(feature = "perf")]
                if let Some(started) = input_time {
                    ui_perf.record_input_to_flush(started.elapsed());
                    recorded_frames += 1;
                    if recorded_frames.is_multiple_of(PERF_LOG_INTERVAL) {
                        log_input_latency(&ui_perf);
                    }
                }
            }

            batch = apc_rx.recv() => {
                let Some(batch) = batch else { break };
                let mut stdout = io::stdout();
                // Wrap the batch, and any it coalesces via drain_apc, in a
                // synchronized update so an async scene re-emit or multi-KB pool
                // fill commits atomically. The existing flush carries the ESU.
                queue!(stdout, BeginSynchronizedUpdate)?;
                stdout.write_all(&batch)?;
                drain_apc(apc_rx)?;
                queue!(stdout, EndSynchronizedUpdate)?;
                stdout.flush()?;
            }
        }
    }

    // Final summary so a short-lived session still reports its latency.
    #[cfg(feature = "perf")]
    log_input_latency(&ui_perf);

    Ok(())
}

/// How long to wait before giving up on a terminal that answers neither query
/// the handshake sends.
///
/// A fallback rather than a budget for the round trip. The cursor-position
/// report is what ends the wait on a foreign terminal and the ident reply is
/// what ends it on a stoatty, so link latency never decides the verdict and
/// this only has to be longer than any terminal takes to answer at all.
const HANDSHAKE_FALLBACK: Duration = Duration::from_secs(2);

/// Announce this editor to the terminal and report whether a stoatty answered.
///
/// Writes an APC hello frame identifying this process followed by a
/// cursor-position query, then reads raw stdin until one of them is answered.
/// A stoatty answers both through one queue, in that order. Every other
/// terminal answers only the second. So the report arriving with no ident ahead
/// of it settles the question, however slow the link, rather than a timeout
/// guessing at it.
///
/// The hello goes out unconditionally, since an APC frame degrades to nothing
/// in a foreign terminal. A stdin that is not a tty cannot carry an answer back
/// at all, so that case reports `false` without probing or waiting.
///
/// Returns whether a stoatty answered, and every byte read that was neither
/// answer. Those are what someone typed while this owned fd 0, and the caller
/// replays them rather than losing what was typed at launch. Crossterm cannot
/// own stdin until this window closes, since its parser has no way to surface
/// an APC reply, which is why the handshake reads fd 0 directly first.
fn stoatty_handshake() -> (bool, Vec<u8>) {
    let hello = command::encode_hello(&HelloCommand {
        pid: std::process::id(),
        log_id: stoat_log::ident::get()
            .map(|ident| ident.id.to_string())
            .unwrap_or_default(),
        hostname: stoat_log::ident::hostname(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });

    let probing = stdin_is_tty();
    {
        // The query rides the same flush as the hello, so the terminal queues
        // both answers together and the order between them is its own. Sent
        // only when something can answer, since an unread report would sit in
        // the terminal's input for whatever runs next.
        let mut stdout = io::stdout().lock();
        let wrote = match probing {
            true => stdout
                .write_all(&hello)
                .and_then(|()| stdout.write_all(b"\x1b[6n")),
            false => stdout.write_all(&hello),
        };
        if wrote.is_err() || stdout.flush().is_err() {
            return (false, Vec::new());
        }
    }
    if !probing {
        tracing::info!("stdin is not a tty, so no terminal can answer the handshake");
        return (false, Vec::new());
    }

    let (reply, leftover) = read_ident_reply(HANDSHAKE_FALLBACK);
    match &reply {
        Some(reply) => tracing::info!(
            stoatty_pid = reply.pid,
            stoatty_log_id = %reply.log_id,
            stoatty_hostname = %reply.hostname,
            stoatty_version = %reply.version,
            "stoatty ident"
        ),
        None => tracing::info!("no stoatty ident reply (headless or foreign terminal)"),
    }
    (reply.is_some(), leftover)
}

/// What the bytes read so far say about the terminal on the other end.
#[derive(Debug, PartialEq, Eq)]
enum Handshake {
    /// A stoatty answered with its ident.
    Stoatty(IdentReply),
    /// The cursor-position query came back with no ident ahead of it, which
    /// only a terminal that ignored the hello does.
    Foreign,
    /// Neither answer has arrived in full yet.
    Pending,
}

/// Read the handshake's answer out of `buf`, removing the bytes it accounted
/// for and leaving the rest.
///
/// What remains is what someone typed while the probe was running, which the
/// caller replays. Both answers are taken out so none of a terminal's own
/// chatter is replayed as input.
///
/// An APC frame that is not an ident reply is consumed and ignored, since it is
/// some other terminal-to-program message and not the answer being waited on.
fn scan_handshake(buf: &mut Vec<u8>) -> Handshake {
    if let Some(span) = vt_input::apc_span(buf) {
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
fn cpr_span(bytes: &[u8]) -> Option<Range<usize>> {
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
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdin_is_tty() -> bool {
    false
}

/// Read raw stdin until the terminal answers one of the handshake's two
/// queries, or `fallback` elapses, returning the ident reply if a stoatty was
/// the one that answered.
///
/// The wait ends on an answer rather than on the clock, so a slow link delays
/// startup by its own round trip instead of being misread as a foreign
/// terminal. The elapsed case is a terminal that answered neither.
#[cfg(unix)]
fn read_ident_reply(fallback: Duration) -> (Option<IdentReply>, Vec<u8>) {
    let deadline = Instant::now() + fallback;
    let mut buf = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
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

/// Frames between periodic input-to-flush latency log lines.
#[cfg(feature = "perf")]
const PERF_LOG_INTERVAL: usize = 600;

/// Log the input-to-flush latency percentiles to `stoat::perf`, in
/// microseconds. A no-op until at least one input-driven frame has flushed.
#[cfg(feature = "perf")]
fn log_input_latency(perf: &crate::perf::PerfStats) {
    if let Some(stats) = perf.input_to_flush_stats() {
        tracing::info!(
            target: "stoat::perf",
            last_us = stats.last / 1_000,
            p50_us = stats.p50 / 1_000,
            p95_us = stats.p95 / 1_000,
            worst_us = stats.worst / 1_000,
            "input-to-flush latency",
        );
    }
}

/// Forward every event `read` produces onto `event_tx` until either end goes
/// away.
///
/// `read` is [`crossterm::event::read`] in production, blocking until stdin has
/// an event. It is a parameter so the loop's two exits can be driven without
/// a terminal.
///
/// Returns when a send fails, meaning the main loop is gone, or when `read`
/// errors. Neither is recoverable from here, and the caller's thread has
/// nothing else to do.
fn forward_input(event_tx: &UnboundedSender<Event>, mut read: impl FnMut() -> io::Result<Event>) {
    while let Ok(event) = read() {
        if event_tx.send(event).is_err() {
            break;
        }
    }
}

/// Write every APC byte batch already queued on `apc_rx` to stdout without
/// blocking, then flush.
///
/// Drains only the currently-queued batches. A batch arriving mid-drain is
/// handled on the next loop wake. Ordered and lossless, unlike the render watch,
/// so `fill` page content is never coalesced or dropped.
fn drain_apc(apc_rx: &mut UnboundedReceiver<Vec<u8>>) -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut wrote = false;
    while let Ok(batch) = apc_rx.try_recv() {
        stdout.write_all(&batch)?;
        wrote = true;
    }
    if wrote {
        stdout.flush()?;
    }
    Ok(())
}

/// Copy `src` into `dst` over their overlapping top-left region, leaving `dst`'s
/// area unchanged.
///
/// `dst` is the terminal's buffer, sized by ratatui's autoresize to the live
/// terminal dimensions; `src` is the frame the main thread rendered, which can
/// lag a frame during a resize and still carry the previous dimensions. The two
/// must never be reconciled by resizing `dst`: ratatui flushes by diffing `dst`
/// against its sibling buffer (also held at the live size), and a differing
/// origin or width there panics. Copying only the intersection keeps that diff
/// valid; any uncovered margin stays blank for the one frame until the main
/// thread re-renders at the new size.
fn copy_clamped(dst: &mut Buffer, src: &Buffer) {
    dst.reset();

    let cols = dst.area.width.min(src.area.width);
    let rows = dst.area.height.min(src.area.height);
    for y in 0..rows {
        for x in 0..cols {
            dst[(x, y)] = src[(x, y)].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident_frame() -> Vec<u8> {
        command::encode_ident_reply(&IdentReply {
            pid: 7,
            log_id: "abc".into(),
            hostname: "host".into(),
            version: "1.2.3".into(),
        })
    }

    fn ident() -> IdentReply {
        IdentReply {
            pid: 7,
            log_id: "abc".into(),
            hostname: "host".into(),
            version: "1.2.3".into(),
        }
    }

    /// The probe's verdict comes from what arrived, and both answers leave the
    /// buffer so only what was typed is replayed.
    ///
    /// The cursor-position report is what makes this a probe rather than a
    /// guess. A stoatty queues its ident ahead of the report, so a report with
    /// no ident before it means no stoatty, however slow the link was.
    #[test]
    fn the_probe_reads_a_verdict_out_of_what_arrived() {
        let cpr = b"\x1b[24;80R".to_vec();
        let cases: Vec<(&str, Vec<u8>, Handshake, &[u8])> = vec![
            (
                "a stoatty answers the ident first, then the report",
                [ident_frame(), cpr.clone()].concat(),
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
                [b"ab".to_vec(), ident_frame(), b"cd".to_vec(), cpr.clone()].concat(),
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
                [b"\x1b[A".to_vec(), cpr.clone()].concat(),
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

    /// Every event read reaches the channel, and a reader that goes away ends
    /// the loop.
    ///
    /// The loop runs on a thread nothing joins, so the exits are the only thing
    /// that ever ends it. One that stopped working would leave a thread reading
    /// stdin for a session that is over, taking keystrokes meant for whatever
    /// runs next.
    #[test]
    fn forwarding_ends_when_the_receiver_is_gone() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut queued = vec![Event::Resize(80, 24), Event::Resize(100, 30)].into_iter();
        // Counted past exhaustion so a loop that reads through the error trips
        // here rather than spinning forever on a reader that never runs out.
        let mut past_end = 0;
        forward_input(&tx, || match queued.next() {
            Some(event) => Ok(event),
            None => {
                past_end += 1;
                assert_eq!(
                    past_end, 1,
                    "a read error must end the loop, not be retried"
                );
                Err(io::Error::from(io::ErrorKind::UnexpectedEof))
            },
        });

        let mut got = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(event);
        }
        assert_eq!(
            got,
            vec![Event::Resize(80, 24), Event::Resize(100, 30)],
            "a read error ends the loop, after everything before it went out",
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        // Bounded so a loop that never notices the closed channel ends here
        // rather than spinning on a read that always succeeds.
        let mut reads = 0;
        forward_input(&tx, || {
            reads += 1;
            match reads > 4 {
                true => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                false => Ok(Event::Resize(80, 24)),
            }
        });
        assert_eq!(
            reads, 1,
            "and a closed channel ends it on the first failed send, not later",
        );
    }

    #[test]
    fn copy_clamped_keeps_dst_area_and_copies_overlap_when_src_is_larger() {
        let src = Buffer::with_lines(["abc", "def"]);
        let mut dst = Buffer::with_lines(["ZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["ab"]));
    }

    #[test]
    fn copy_clamped_clears_stale_margin_when_src_is_smaller() {
        let src = Buffer::with_lines(["xy"]);
        let mut dst = Buffer::with_lines(["ZZZ", "ZZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["xy ", "   "]));
    }
}
