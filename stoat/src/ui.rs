//! UI thread: owns the terminal and stdin event stream.
//!
//! Runs on a dedicated OS thread with its own single-threaded tokio runtime.
//! Its only job is shuttling bytes -- forwarding input events to the main thread
//! and flushing rendered buffers to the terminal. Physical thread isolation
//! guarantees that terminal IO latency is independent of main-thread workload.

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute, queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use futures::StreamExt;
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    backtrace::Backtrace,
    io::{self, Write},
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
    mpsc::{Sender, UnboundedReceiver},
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
    pub input_time: Option<std::time::Instant>,
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

pub fn spawn(
    event_tx: Sender<Event>,
    mut render_rx: watch::Receiver<Option<RenderFrame>>,
    mut apc_rx: UnboundedReceiver<Vec<u8>>,
    mouse_captured: bool,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        rt.block_on(async move {
            let mut terminal = ratatui::init();
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            }
            let result = run(&event_tx, &mut render_rx, &mut apc_rx, &mut terminal).await;
            if mouse_captured {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
            ratatui::restore();
            result
        })
    })
}

async fn run(
    event_tx: &Sender<Event>,
    render_rx: &mut watch::Receiver<Option<RenderFrame>>,
    apc_rx: &mut UnboundedReceiver<Vec<u8>>,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    // Main thread needs terminal dimensions before it can render the first frame
    let size = terminal.size()?;
    if event_tx
        .send(Event::Resize(size.width, size.height))
        .await
        .is_err()
    {
        return Ok(());
    }

    stoatty_handshake();

    let mut events = EventStream::new();

    // Reused so a redraw copies into this buffer's allocation instead of
    // cloning a fresh one out of the watch each frame.
    let mut frame = Buffer::empty(Rect::new(0, 0, size.width, size.height));

    // UI-thread-local input-to-flush latency, logged periodically. The main
    // thread keeps its own PerfStats, so this needs no cross-thread channel.
    #[cfg(feature = "perf")]
    let mut ui_perf = crate::perf::PerfStats::default();
    #[cfg(feature = "perf")]
    let mut recorded_frames: usize = 0;

    loop {
        // Biased: always drain input before flushing frames so keypresses
        // are never starved by a burst of render buffers
        tokio::select! {
            biased;

            event = events.next() => {
                let Some(event) = event else { break };
                let event = event?;
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }

            changed = render_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                // Open a synchronized update so the cell diff, undercurl re-stamp,
                // and APC batch below commit as one frame. Queued rather than
                // flushed, so ratatui's draw flush carries it ahead of the cell
                // diff in the same write.
                queue!(io::stdout(), BeginSynchronizedUpdate)?;
                // Copy the latest frame into `frame` and drop the watch borrow
                // before drawing, so the slow terminal flush never holds the
                // lock the render thread needs to publish the next frame.
                #[cfg(feature = "perf")]
                let mut input_time = None;
                let (cursor, undercurl) = {
                    let latest = render_rx.borrow_and_update();
                    match latest.as_ref() {
                        Some(src) => {
                            frame.resize(src.buffer.area);
                            frame.content.clone_from(&src.buffer.content);
                            #[cfg(feature = "perf")]
                            {
                                input_time = src.input_time;
                            }
                            (Some(src.cursor), src.undercurl.clone())
                        },
                        None => (None, Vec::new()),
                    }
                };
                if let Some(cursor) = cursor {
                    terminal.draw(|f| {
                        let dst = f.buffer_mut();
                        if dst.area == frame.area {
                            dst.content.clone_from(&frame.content);
                        } else {
                            copy_clamped(dst, &frame);
                        }
                        if let Some((col, row)) = cursor {
                            f.set_cursor_position((col, row));
                        }
                    })?;
                }
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

/// How long to wait for the terminal's ident reply before giving up.
///
/// Sized to cover an ssh round trip while bounding the startup window during
/// which typed keystrokes are consumed here and lost.
const IDENT_REPLY_TIMEOUT: Duration = Duration::from_millis(250);

/// Announce this editor to the terminal and log its ident reply.
///
/// Writes an APC hello frame identifying this process, then reads raw stdin for
/// up to [`IDENT_REPLY_TIMEOUT`] for the terminal's ident reply. The hello is
/// sent unconditionally because an APC frame degrades to nothing in a foreign
/// terminal, so a missing reply is the normal headless, ssh, or
/// foreign-terminal case.
///
/// Any keystrokes typed during the read window are consumed here and lost. This
/// wart is tolerated because crossterm's [`EventStream`] cannot surface an APC
/// reply and must not own stdin until the window closes, so the handshake reads
/// fd 0 directly first.
fn stoatty_handshake() {
    let hello = command::encode_hello(&HelloCommand {
        pid: std::process::id(),
        log_id: stoat_log::ident::get()
            .map(|ident| ident.id.to_string())
            .unwrap_or_default(),
        hostname: stoat_log::ident::hostname(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });

    {
        let mut stdout = io::stdout().lock();
        if stdout.write_all(&hello).is_err() || stdout.flush().is_err() {
            return;
        }
    }

    match read_ident_reply(IDENT_REPLY_TIMEOUT) {
        Some(reply) => tracing::info!(
            stoatty_pid = reply.pid,
            stoatty_log_id = %reply.log_id,
            stoatty_hostname = %reply.hostname,
            stoatty_version = %reply.version,
            "stoatty ident"
        ),
        None => tracing::info!("no stoatty ident reply (headless or foreign terminal)"),
    }
}

/// Read raw stdin for up to `timeout`, returning the terminal's ident reply if
/// one arrives within it. Bytes that are not the reply frame are dropped.
#[cfg(unix)]
fn read_ident_reply(timeout: Duration) -> Option<IdentReply> {
    let deadline = Instant::now() + timeout;
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

        if let Some(span) = extract_apc_payload(&buf) {
            return frame::decode(span).and_then(|frame| command::decode_ident_reply(&frame));
        }
    }

    if !buf.is_empty() {
        tracing::debug!(
            dropped = buf.len(),
            "dropped non-APC stdin bytes during handshake"
        );
    }
    None
}

#[cfg(not(unix))]
fn read_ident_reply(_timeout: Duration) -> Option<IdentReply> {
    None
}

/// The first complete APC frame span in `bytes`, from `ESC _` through its `ESC \`
/// or `BEL` terminator inclusive, or `None` if no complete span is present yet.
///
/// Leading bytes before the introducer (stray keystrokes) are skipped.
fn extract_apc_payload(bytes: &[u8]) -> Option<&[u8]> {
    let start = bytes.windows(2).position(|pair| pair == b"\x1b_")?;
    let rest = &bytes[start..];
    let mut i = 2;
    while i < rest.len() {
        if rest[i] == 0x07 {
            return Some(&rest[..=i]);
        }
        if rest[i] == 0x1b && rest.get(i + 1) == Some(&b'\\') {
            return Some(&rest[..=i + 1]);
        }
        i += 1;
    }
    None
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

/// Write every APC byte batch already queued on `apc_rx` to stdout without
/// blocking, then flush.
///
/// Drains only the currently-queued batches; a batch arriving mid-drain is
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

    #[test]
    fn copy_clamped_keeps_dst_area_and_copies_overlap_when_src_is_larger() {
        let src = Buffer::with_lines(["abc", "def"]);
        let mut dst = Buffer::with_lines(["ZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["ab"]));
    }

    #[test]
    fn extract_apc_payload_returns_a_complete_st_span() {
        let bytes = b"\x1b_Gstoatty;ident\x1b\\";
        assert_eq!(extract_apc_payload(bytes), Some(&bytes[..]));
    }

    #[test]
    fn extract_apc_payload_skips_leading_garbage_and_accepts_bel() {
        assert_eq!(
            extract_apc_payload(b"junk\x1b_Gstoatty;ident\x07"),
            Some(&b"\x1b_Gstoatty;ident\x07"[..])
        );
    }

    #[test]
    fn extract_apc_payload_is_none_when_the_span_is_incomplete() {
        assert_eq!(extract_apc_payload(b"\x1b_Gstoatty;ident"), None);
    }

    #[test]
    fn copy_clamped_clears_stale_margin_when_src_is_smaller() {
        let src = Buffer::with_lines(["xy"]);
        let mut dst = Buffer::with_lines(["ZZZ", "ZZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["xy ", "   "]));
    }
}
