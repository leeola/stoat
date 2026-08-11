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
/// Only the perf build times frames, so this would be an unused import without
/// it.
#[cfg(feature = "perf")]
use std::time::Instant;
use std::{
    backtrace::Backtrace,
    io::{self, Write},
    panic,
    sync::{Arc, Once},
    thread,
};
use stoatty_protocol::{
    command::{self, HelloCommand},
    detect,
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

/// `stoatty_tx` carries the ident handshake's one-shot answer to the app: the
/// peer's protocol version when a stoatty replied, `None` when none did. The
/// app cannot learn it any other way, since the handshake needs raw mode and
/// sole ownership of fd 0, both of which live on this thread.
pub fn spawn(
    event_tx: UnboundedSender<Event>,
    mut render_rx: watch::Receiver<Option<RenderFrame>>,
    mut apc_rx: UnboundedReceiver<Vec<u8>>,
    stoatty_tx: UnboundedSender<Option<u32>>,
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
    stoatty_tx: &UnboundedSender<Option<u32>>,
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
                drain_apc(None, apc_rx)?;
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
                // Wrap the batch, and any it drains alongside it, in a
                // synchronized update so an async scene re-emit or multi-KB pool
                // fill commits atomically. The drain flush carries the ESU.
                queue!(io::stdout(), BeginSynchronizedUpdate)?;
                drain_apc(Some(batch), apc_rx)?;
                queue!(io::stdout(), EndSynchronizedUpdate)?;
                io::stdout().flush()?;
            }
        }
    }

    // Final summary so a short-lived session still reports its latency.
    #[cfg(feature = "perf")]
    log_input_latency(&ui_perf);

    Ok(())
}

/// Announce this editor to the terminal and report whether a stoatty answered.
///
/// Thin wrapper over [`detect::handshake`], which does the probing. This
/// supplies stoat's own identity, logs the result, and narrows the reply to the
/// peer's protocol version, which is all the app keeps.
///
/// Returns that version when a stoatty answered and `None` when none did, plus
/// every byte read that was neither answer. Those are what someone typed while
/// the probe owned fd 0, and the caller replays them rather than losing what was
/// typed at launch.
fn stoatty_handshake() -> (Option<u32>, Vec<u8>) {
    let hello = HelloCommand {
        pid: std::process::id(),
        log_id: stoat_log::ident::get()
            .map(|ident| ident.id.to_string())
            .unwrap_or_default(),
        hostname: stoat_log::ident::hostname(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: stoatty_protocol::PROTOCOL_VERSION,
    };

    let (reply, leftover) = detect::handshake(&hello, detect::HANDSHAKE_FALLBACK);
    match &reply {
        Some(reply) => tracing::info!(
            stoatty_pid = reply.pid,
            stoatty_log_id = %reply.log_id,
            stoatty_hostname = %reply.hostname,
            stoatty_version = %reply.version,
            stoatty_protocol = reply.protocol,
            "stoatty ident"
        ),
        None => tracing::info!("no stoatty ident reply (headless or foreign terminal)"),
    }
    (reply.map(|reply| reply.protocol), leftover)
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

/// Write `first`, then every APC byte batch already queued on `apc_rx`, to
/// stdout, and flush if anything went out.
///
/// Drains only the currently-queued batches. A batch arriving mid-drain is
/// handled on the next loop wake.
///
/// Ordered, and lossless for everything whose content this cannot reason
/// about. The exception is a pool fill superseded by a later queued one for the
/// same page, which [`supersede_stale_fills`] drops.
fn drain_apc(first: Option<Vec<u8>>, apc_rx: &mut UnboundedReceiver<Vec<u8>>) -> io::Result<()> {
    let mut queued: Vec<Vec<u8>> = first.into_iter().collect();
    while let Ok(batch) = apc_rx.try_recv() {
        queued.push(batch);
    }
    if queued.is_empty() {
        return Ok(());
    }

    supersede_stale_fills(&mut queued);

    let mut stdout = io::stdout();
    for batch in &queued {
        stdout.write_all(batch)?;
    }
    stdout.flush()
}

/// Drop each queued pool fill that a later one for the same page replaces,
/// leaving everything else where it was.
///
/// A pool slot is last-writer-wins and no queued command reads a page's
/// content, so an earlier fill for a page written again later is work nobody
/// will ever see. On a link slower than the emit rate a hard scroll queues
/// megabytes of them, and without this the user watches seconds-old pages
/// replay in order before the current one arrives.
///
/// Only fills are touched. Anything else is content this cannot reason about,
/// so it keeps its place and its bytes. Walking backward is what makes the
/// survivor the last of each page rather than the first.
fn supersede_stale_fills(queued: &mut Vec<Vec<u8>>) {
    let mut seen: Vec<(u32, u64)> = Vec::new();
    let mut superseded = vec![false; queued.len()];

    for (at, batch) in queued.iter().enumerate().rev() {
        let Some(key) = command::fill_batch_key(batch) else {
            continue;
        };
        match seen.contains(&key) {
            true => superseded[at] = true,
            false => seen.push(key),
        }
    }

    let mut at = 0;
    queued.retain(|_| {
        at += 1;
        !superseded[at - 1]
    });
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

    /// A queued fill goes out only if nothing later replaces its page, and
    /// everything else goes out untouched and in order.
    ///
    /// The dropping is what bounds the replay debt a slow link builds. Without
    /// it a hard scroll queues megabytes of pages, and the user watches
    /// seconds-old ones arrive in order before the current one. It is safe only
    /// for fills, since a pool slot is last-writer-wins and nothing queued
    /// reads a page's content, so anything else keeps its place.
    #[test]
    fn a_drain_writes_only_the_last_fill_of_each_page() {
        let fill = |pool: u32, index: u64, tag: &str| {
            let mut out = Vec::new();
            command::encode_fill_into(&mut out, pool, index);
            out.extend_from_slice(tag.as_bytes());
            command::encode_fill_end_into(&mut out);
            out
        };
        let other = |tag: &str| {
            let mut out = Vec::new();
            command::encode_pool_cursor_into(
                &mut out,
                &command::PoolCursorCommand {
                    pool: 1,
                    row: 0,
                    col: 0,
                },
            );
            out.extend_from_slice(tag.as_bytes());
            out
        };

        let mut queued = vec![
            fill(1, 7, "stale"),
            other("keep me"),
            fill(1, 8, "another page"),
            fill(1, 7, "current"),
            other("keep me too"),
            fill(2, 7, "another pool"),
        ];
        supersede_stale_fills(&mut queued);

        assert_eq!(
            queued,
            vec![
                other("keep me"),
                fill(1, 8, "another page"),
                fill(1, 7, "current"),
                other("keep me too"),
                fill(2, 7, "another pool"),
            ],
            "only the earlier fill of the repeated page goes, and order holds",
        );

        let mut alone = vec![fill(1, 7, "the only one")];
        supersede_stale_fills(&mut alone);
        assert_eq!(
            alone,
            vec![fill(1, 7, "the only one")],
            "and a drain with nothing to supersede coalesces nothing",
        );
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
