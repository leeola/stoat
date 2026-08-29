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

use crate::{
    render::undercurl::UndercurlStamp,
    ssh::{PassthroughSlot, SlotState, UiControl},
    vt_input,
};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    },
    execute, queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen},
};
use futures::FutureExt;
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
    time::Duration,
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

/// Every channel end the UI thread owns, handed over in one piece.
///
/// The thread holds them as a set for its whole life, and spelling each one out
/// at both entry points buries the two parameters that actually vary.
pub struct UiChannels {
    /// Terminal input on its way to the app.
    pub event_tx: UnboundedSender<Event>,
    /// The latest painted frame, latest-wins.
    pub render_rx: watch::Receiver<Option<RenderFrame>>,
    /// Ordered stoatty APC byte batches, written after each grid frame.
    pub apc_rx: UnboundedReceiver<Vec<u8>>,
    /// The ident handshake's one-shot answer to the app: the peer's protocol
    /// version when a stoatty replied, `None` when none did. The app learns it
    /// no other way, since the handshake needs raw mode and sole ownership of
    /// fd 0, both of which live on this thread.
    pub stoatty_tx: UnboundedSender<Option<u32>>,
    /// The tty's cell size, for the same reason, and unlike the handshake it
    /// goes again on every resize, since a window that changed size or font has
    /// changed the answer.
    pub cell_pixels_tx: UnboundedSender<Option<(u16, u16)>>,
    /// What fd 0 feeds, which `:ssh` moves to a remote session and back.
    pub slot: Arc<PassthroughSlot>,
    /// What to do with the screen while a remote session owns it.
    pub ui_rx: UnboundedReceiver<UiControl>,
}

pub fn spawn(mut channels: UiChannels, mouse_captured: bool) -> thread::JoinHandle<io::Result<()>> {
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
            let result = run(&mut channels, &mut terminal, mouse_captured).await;
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
    channels: &mut UiChannels,
    terminal: &mut ratatui::DefaultTerminal,
    mouse_captured: bool,
) -> io::Result<()> {
    let UiChannels {
        event_tx,
        render_rx,
        apc_rx,
        stoatty_tx,
        cell_pixels_tx,
        slot,
        ui_rx,
    } = channels;
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
    let _ = cell_pixels_tx.send(tty_cell_pixels());

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
        let cell_pixels_tx = cell_pixels_tx.clone();
        let stoatty_tx = stoatty_tx.clone();
        let slot = slot.clone();
        move || {
            forward_input(
                InputChannels {
                    event_tx: &event_tx,
                    cell_pixels_tx: &cell_pixels_tx,
                    stoatty_tx: &stoatty_tx,
                    slot: &slot,
                },
                crossterm::event::poll,
                crossterm::event::read,
                read_stdin_raw,
                tty_winsize,
                stoatty_handshake,
            )
        }
    });

    // What the terminal carries, so a frame whose squiggle is already on screen
    // writes nothing. Lives here rather than beside the paint because only this
    // thread knows which frames reached the terminal.
    let mut stamp = UndercurlStamp::default();

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

            // Ahead of the render arm on purpose. The app publishes a frame
            // right after it sends Resume, and a frame drawn before the
            // alternate screen is back lands on the shell's screen with nothing
            // to redraw it.
            Some(ctl) = ui_rx.recv() => {
                match ctl {
                    // No synchronized-update wrapper. The remote frames its own,
                    // and a local wrapper closing mid-frame tears it.
                    UiControl::Raw(bytes) => {
                        let mut stdout = io::stdout();
                        stdout.write_all(&bytes)?;
                        stdout.flush()?;
                    },
                    // The remote's exit left the alternate screen, which took
                    // this session's screen with it. Raw mode is local termios
                    // the remote's bytes never touched, so it is not re-applied.
                    UiControl::Resume => {
                        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
                        if mouse_captured {
                            execute!(io::stdout(), EnableMouseCapture)?;
                        }
                        terminal.clear()?;
                        stamp = UndercurlStamp::default();
                    },
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
                        // Compared here rather than after the draw so an
                        // unchanged squiggle never leaves the watch at all.
                        let restamp = stamp.advance(src.buffer.area, &src.undercurl);
                        (src.buffer.clone(), src.cursor, restamp)
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
                    None => None,
                };
                // Re-stamp diagnostic curly underlines over the grid just drawn,
                // before the APC batches composite over the same stdout.
                if let Some(bytes) = undercurl
                    && !bytes.is_empty()
                {
                    let mut stdout = io::stdout();
                    stdout.write_all(bytes)?;
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

/// One cell's pixel size, from a winsize the tty reported.
///
/// The winsize gives the whole text area and the grid it holds, so a cell is
/// the quotient. `None` when any field is zero, which is what a terminal that
/// does not report pixels sends, and is also the only division there is nothing
/// to do about.
fn cell_pixels_from_winsize(ws: &libc::winsize) -> Option<(u16, u16)> {
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

/// Ask the tty how many pixels a cell is, or `None` when it will not say.
///
/// An image client sizes what it draws from this, and a terminal that reports
/// no pixels leaves it with nothing to divide by. Most do report them, and the
/// stoatty this usually runs inside always does.
fn tty_cell_pixels() -> Option<(u16, u16)> {
    cell_pixels_from_winsize(&tty_winsize()?)
}

/// The tty text area and the grid it holds, or `None` when the ioctl fails.
///
/// Separate from [`tty_cell_pixels`] because the passthrough loop needs the cell
/// counts rather than the pixel quotient: crossterm reports no resize while a
/// remote session owns fd 0, so the size is polled instead.
fn tty_winsize() -> Option<libc::winsize> {
    // SAFETY: TIOCGWINSZ writes a winsize through the pointer and reads nothing
    // else. The struct is fully initialized before the call, so a driver that
    // writes none of it still leaves defined bytes behind.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &raw mut ws) != 0 {
            return None;
        }
        Some(ws)
    }
}

/// How long each wait for input runs before the loop re-reads the slot.
///
/// The state changes on another thread, so the bound is how long a `:ssh` waits
/// to be noticed, and it doubles as the tick that polls the window size while a
/// remote session owns fd 0.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Bytes read per pass while a remote session owns fd 0. Large enough that a
/// pasted screenful crosses in one write.
const RAW_CHUNK: usize = 4096;

/// Every channel end and shared handle the input thread reads or writes.
///
/// Grouped for the same reason as [`UiChannels`]. The thread holds them for its
/// whole life, and spelling four of them out at every call buries the closures
/// that actually differ between a real run and a test.
struct InputChannels<'a> {
    /// Terminal input on its way to the app.
    event_tx: &'a UnboundedSender<Event>,
    /// The tty's cell size, re-read on every resize and after an attach.
    cell_pixels_tx: &'a UnboundedSender<Option<(u16, u16)>>,
    /// The ident handshake's answer. Sent once at startup by the UI thread, and
    /// again from here for each terminal that attaches later.
    stoatty_tx: &'a UnboundedSender<Option<u32>>,
    /// What fd 0 feeds, which the app moves between the editor, a remote
    /// session, and a handshake.
    slot: &'a PassthroughSlot,
}

/// Forward what fd 0 produces, to the app or to the remote session the slot
/// names.
///
/// Four modes, re-read every pass so a `:ssh` or an attach takes effect without
/// a wake:
///
/// - Idle: `poll` then `read` an event and send it, as a normal session does.
/// - Pending: acknowledge that fd 0 is no longer parsed, then buffer raw bytes. The app spawns ssh
///   on that ack, so anything typed in between is the user's and is handed to the session once it
///   exists.
/// - Active: write every raw byte to the session, and poll the window size, because crossterm
///   reports no resize while nothing parses fd 0.
/// - Handshake: identify the terminal that just attached and report it, then go idle. The probe
///   reads raw fd 0, so it belongs on this thread and nowhere else.
///
/// Sound because crossterm 0.29 reads fd 0 on the calling thread, so nothing
/// else reads stdin while this one does.
///
/// `poll`, `read`, `read_raw`, and `winsize` are the crossterm and libc calls in
/// production, and parameters so the loop runs without a terminal.
///
/// Returns when a send fails, meaning the main loop is gone, or when a read
/// errors. Neither is recoverable from here, and the caller's thread has
/// nothing else to do.
fn forward_input(
    channels: InputChannels<'_>,
    mut poll: impl FnMut(Duration) -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<Event>,
    mut read_raw: impl FnMut(&mut [u8], Duration) -> io::Result<usize>,
    mut winsize: impl FnMut() -> Option<libc::winsize>,
    mut handshake: impl FnMut() -> (Option<u32>, Vec<u8>),
) {
    let InputChannels {
        event_tx,
        cell_pixels_tx,
        stoatty_tx,
        slot,
    } = channels;

    let mut raw = [0u8; RAW_CHUNK];
    let mut buffered: Vec<u8> = Vec::new();
    let mut acked = false;
    let mut ran_session = false;
    let mut last_size: Option<(u16, u16)> = None;

    loop {
        match slot.state() {
            SlotState::Idle => {
                // The app laid out against whatever grid it last heard about,
                // and the window is free to have changed size while the remote
                // owned it, so the current size goes out before anything else.
                if ran_session {
                    ran_session = false;
                    buffered.clear();
                    last_size = None;
                    if let Some(ws) = winsize()
                        && event_tx.send(Event::Resize(ws.ws_col, ws.ws_row)).is_err()
                    {
                        return;
                    }
                    let _ = cell_pixels_tx.send(tty_cell_pixels());
                }
                acked = false;

                match poll(INPUT_POLL) {
                    Ok(true) => {},
                    Ok(false) => continue,
                    Err(_) => return,
                }
                let Ok(event) = read() else {
                    return;
                };
                // A resized window has a new cell count and may have a new cell
                // size, and the tty answers for both. Read before forwarding, so
                // the app has the new size by the time it lays out against the
                // new grid.
                if matches!(event, Event::Resize(..)) {
                    let _ = cell_pixels_tx.send(tty_cell_pixels());
                }
                if event_tx.send(event).is_err() {
                    return;
                }
            },
            SlotState::Pending => {
                if !acked {
                    acked = true;
                    slot.ack();
                }
                match read_raw(&mut raw, INPUT_POLL) {
                    Ok(0) => {},
                    Ok(n) => buffered.extend_from_slice(&raw[..n]),
                    Err(_) => return,
                }
            },
            SlotState::Active(session) => {
                ran_session = true;
                acked = false;
                if !buffered.is_empty() {
                    let held = std::mem::take(&mut buffered);
                    let _ = session.write(&held).now_or_never();
                }
                match read_raw(&mut raw, INPUT_POLL) {
                    Ok(0) => {
                        if let Some(ws) = winsize() {
                            let size = (ws.ws_row, ws.ws_col);
                            if last_size != Some(size) {
                                last_size = Some(size);
                                let _ = session.resize(ws.ws_row, ws.ws_col);
                            }
                        }
                    },
                    Ok(n) => {
                        let _ = session.write(&raw[..n]).now_or_never();
                    },
                    Err(_) => return,
                }
            },
            SlotState::Handshake => {
                let (stoatty, typed) = handshake();
                if stoatty_tx.send(stoatty).is_err() {
                    return;
                }
                // Typed while the probe owned fd 0, so it belongs to the user
                // and goes ahead of anything typed after, exactly as at launch.
                for event in vt_input::decode(&typed) {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                }
                // The app laid out against the terminal that left, so the Idle
                // re-entry owes it this one's size and cell metrics.
                ran_session = true;
                slot.release();
            },
        }
    }
}

/// Read what is on fd 0 into `buf`, waiting up to `timeout`.
///
/// Zero means the wait elapsed with nothing to read, which is also what a
/// closed stdin reports. Both leave the caller with nothing to forward, and the
/// loop that calls this ends with the session either way.
fn read_stdin_raw(buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll reads and writes the one pollfd through the pointer and
    // touches nothing else. The struct is initialized above.
    let ready = unsafe { libc::poll(&raw mut fds, 1, timeout.as_millis() as libc::c_int) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(0);
    }

    // SAFETY: read writes at most buf.len() bytes through the pointer, which
    // borrows a live slice for the call.
    let got = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(got as usize)
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
            command::encode_fill_scope(&mut out, pool, index, |out| {
                out.extend_from_slice(tag.as_bytes())
            });
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

    /// An idle slot with nothing armed, plus the raw and winsize closures a
    /// non-passthrough run never reaches.
    fn idle_input(
        event_tx: &UnboundedSender<Event>,
        cell_pixels_tx: &UnboundedSender<Option<(u16, u16)>>,
        read: impl FnMut() -> io::Result<Event>,
    ) {
        let (slot, _ack_rx) = PassthroughSlot::new();
        let (stoatty_tx, _stoatty_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_input(
            InputChannels {
                event_tx,
                cell_pixels_tx,
                stoatty_tx: &stoatty_tx,
                slot: &slot,
            },
            |_| Ok(true),
            read,
            |_, _| Ok(0),
            || None,
            || (None, Vec::new()),
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
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        idle_input(&tx, &px_tx, || match queued.next() {
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
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        idle_input(&tx, &px_tx, || {
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

    /// The whole passthrough handoff from the input thread's side.
    ///
    /// The ack is what the app waits for before it spawns ssh, and bytes typed
    /// between the arm and the spawn belong to the remote, so they have to be
    /// held rather than dropped. Coming back, the app has laid out against a
    /// grid it has not heard about since the handoff, so the current size must
    /// reach it unprompted.
    #[test]
    fn passthrough_acks_holds_early_bytes_then_resizes_on_return() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        let (slot, mut ack_rx) = PassthroughSlot::new();
        let session = Arc::new(crate::host::FakeTerminalSession::new());
        slot.arm();

        // Each raw read advances the slot, so the loop walks Pending, Active,
        // and back to Idle in as many passes.
        let mut pass = 0;
        let chunks: [&[u8]; 2] = [b"typed early", b"and later"];
        let (stoatty_tx, _stoatty_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_input(
            InputChannels {
                event_tx: &tx,
                cell_pixels_tx: &px_tx,
                stoatty_tx: &stoatty_tx,
                slot: &slot,
            },
            |_| Ok(true),
            || Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            |buf, _| {
                let chunk = chunks[pass.min(1)];
                buf[..chunk.len()].copy_from_slice(chunk);
                pass += 1;
                if pass == 1 {
                    slot.engage(session.clone());
                }
                if pass == 2 {
                    slot.release();
                }
                Ok(chunk.len())
            },
            || {
                Some(libc::winsize {
                    ws_row: 30,
                    ws_col: 100,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                })
            },
            || (None, Vec::new()),
        );

        assert!(ack_rx.try_recv().is_ok(), "the arm is acknowledged");
        assert_eq!(
            session.sent_strings(),
            vec!["typed early".to_owned(), "and later".to_owned()],
            "bytes held while pending go first, then the ones typed at the remote",
        );

        let mut got = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(event);
        }
        assert_eq!(
            got,
            vec![Event::Resize(100, 30)],
            "and the return reports the size the window reached",
        );
    }

    /// The probe reads raw fd 0, so it belongs on this thread. What was typed
    /// while it owned the terminal is the user's, and the app laid out against
    /// the terminal that left, so the new one's size follows.
    #[test]
    fn a_rehandshake_identifies_the_new_terminal_and_replays_what_was_typed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stoatty_tx, mut stoatty_rx) = tokio::sync::mpsc::unbounded_channel();
        let (slot, _ack_rx) = PassthroughSlot::new();
        slot.rehandshake();

        // The Idle pass after the handshake ends the loop, so the one probe is
        // all this runs.
        forward_input(
            InputChannels {
                event_tx: &tx,
                cell_pixels_tx: &px_tx,
                stoatty_tx: &stoatty_tx,
                slot: &slot,
            },
            |_| Ok(true),
            || Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            |_, _| Ok(0),
            || {
                Some(libc::winsize {
                    ws_row: 40,
                    ws_col: 120,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                })
            },
            || (Some(3), b"x".to_vec()),
        );

        assert_eq!(
            stoatty_rx.try_recv(),
            Ok(Some(3)),
            "the new terminal's protocol reaches the app",
        );
        assert!(
            matches!(slot.state(), SlotState::Idle),
            "and fd 0 goes back to feeding the editor",
        );

        let mut got = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(event);
        }
        assert_eq!(
            got,
            vec![
                Event::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('x'),
                    crossterm::event::KeyModifiers::NONE,
                )),
                Event::Resize(120, 40),
            ],
            "what was typed at the probe goes first, then the new size",
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

    /// The winsize gives the text area and the grid it holds, so a cell is the
    /// quotient of the two.
    #[test]
    fn a_cell_is_the_text_area_over_the_grid() {
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 640,
            ws_ypixel: 384,
        };

        assert_eq!(cell_pixels_from_winsize(&ws), Some((8, 16)));
    }

    /// A terminal that does not report pixels sends zeros, which is also the
    /// only division there is nothing to do about.
    #[test]
    fn a_zero_field_reports_nothing_rather_than_dividing() {
        let full = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 640,
            ws_ypixel: 384,
        };

        for (name, ws) in [
            (
                "no pixel width",
                libc::winsize {
                    ws_xpixel: 0,
                    ..full
                },
            ),
            (
                "no pixel height",
                libc::winsize {
                    ws_ypixel: 0,
                    ..full
                },
            ),
            ("no columns", libc::winsize { ws_col: 0, ..full }),
            ("no rows", libc::winsize { ws_row: 0, ..full }),
        ] {
            assert_eq!(cell_pixels_from_winsize(&ws), None, "{name}");
        }
    }

    /// A text area that does not divide evenly is the ordinary case, since the
    /// window keeps whatever pixels the last cell leaves over.
    #[test]
    fn a_ragged_text_area_reports_the_whole_cells_it_holds() {
        let ws = libc::winsize {
            ws_row: 10,
            ws_col: 10,
            ws_xpixel: 85,
            ws_ypixel: 165,
        };

        assert_eq!(cell_pixels_from_winsize(&ws), Some((8, 16)));
    }
}
