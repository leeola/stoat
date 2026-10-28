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

mod flush;

use crate::{
    render::{undercurl::UndercurlStamp, Hold},
    ssh::{PassthroughSlot, SlotState, UiControl},
    tty, vt_input,
};
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{
        enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    },
};
use flush::{FrameBackend, ShownFrame};
use futures::FutureExt;
use ratatui::{buffer::Buffer, layout::Rect, Terminal};
#[cfg(feature = "perf")]
use std::time::Instant;
use std::{
    backtrace::Backtrace,
    io::{self, PipeReader, Read, Stdout, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    panic,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once,
    },
    thread,
    time::Duration,
};
#[cfg(feature = "perf")]
use stoat_scheduler::Executor;
use stoatty_protocol::{
    command::{self, HelloCommand},
    detect,
};
use tokio::{
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        watch,
    },
    time,
};

/// The longest time the UI thread holds a written frame for its APC batches.
///
/// It is long enough for the pool job that redraws the gutter of the pages a
/// scroll moves, so a scroll reaches the terminal in one write. A refill of
/// many pages takes longer. The frame then goes out without the rest of the
/// refill, so no job holds the screen for longer than this.
const FRAME_BATCH_WAIT: Duration = Duration::from_millis(4);

/// Whether this session pushed its keyboard flags onto the terminal's stack.
///
/// A static, because the panic hook pops them too, and it runs on whichever
/// thread panicked, with no handle to the UI thread's state.
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);

/// One rendered frame published from the main thread to the UI thread.
///
/// Carries the painted grid plus the optional terminal-cursor cell. `cursor`
/// is `Some((col, row))` only when running inside stoatty and the focused
/// document editor or changes view delegates its primary cursor to the
/// terminal cursor. Otherwise it is `None` and the cursor stays hidden, with
/// the editor painting its own cursor cell into `buffer`.
pub struct RenderFrame {
    pub buffer: Arc<Buffer>,
    pub cursor: Option<(u16, u16)>,
    /// Raw VT that re-stamps diagnostic curly underlines over `buffer` after it
    /// is drawn, empty outside stoatty or when no diagnostic span is visible.
    /// Written to stdout right after the grid draw so it decorates the exact
    /// frame it was built for. See [`crate::render::undercurl`].
    pub undercurl: Vec<u8>,
    /// The rects resident pools composite over, where the UI thread's row diff
    /// writes no cell of `buffer`.
    ///
    /// The terminal shows a resident pool's pages at rest, so the live cells
    /// under it never show. The rects ride the frame, so a frame and the rects
    /// it leaves to pools never disagree.
    pub resident: Vec<Rect>,
    /// The pane captures `buffer` holds, which the UI thread never reads.
    ///
    /// They ride back to the run loop with the buffer, as the frame the next
    /// publish replaces, so the blank job that takes the buffer puts each
    /// capture back into its rect.
    pub(crate) holds: Vec<Hold>,
    /// The pane captures `buffer` shows with nothing painted over them, as each
    /// capture's rect and the render tick of the frame that took it.
    ///
    /// No two captures share a rect and a tick, so a rect that the frame on
    /// screen lists with the same tick holds the same cells there, and the UI
    /// thread compares none of them. A popup, a badge, a dock, or a modal
    /// paints over the panes anywhere, so a frame that paints one lists no
    /// capture.
    pub(crate) captures: Vec<(Rect, u64)>,
    /// The APC batches that the run loop sends for this frame after the publish.
    ///
    /// The UI thread waits for them, for at most [`FRAME_BATCH_WAIT`], before
    /// it ends the frame's synchronized update. The frame and its batches then
    /// leave in one write.
    pub(crate) batches: FrameBatches,
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

/// The APC batches that the run loop sends for one published frame.
///
/// The run loop publishes a frame and then sends the frame's batches. Its
/// emits send most of them, and the pool jobs that the emits start on the
/// blocking pool send the rest. Each sender keeps a [`FrameBatchesHold`] until
/// its last batch is on the APC channel. The UI thread waits for every hold to
/// go before it drains the channel, so the frame and its batches go out in one
/// synchronized update.
#[derive(Clone)]
pub(crate) struct FrameBatches(watch::Receiver<()>);

impl FrameBatches {
    /// Starts the batches of a frame, with the first hold on them.
    pub(crate) fn start() -> (FrameBatchesHold, Self) {
        let (sender, receiver) = watch::channel(());
        let hold = FrameBatchesHold {
            _sender: Arc::new(sender),
        };
        (hold, Self(receiver))
    }

    /// Waits until every hold on the frame's batches is gone.
    pub(crate) async fn sent(mut self) {
        // Nothing is ever sent on the channel. The last hold to go drops the
        // sender, which closes the channel and ends the wait.
        let _ = self.0.changed().await;
    }
}

/// A hold on the batches of one frame, which keeps the frame's synchronized
/// update open.
///
/// Drop it after the last batch that it covers is on the APC channel.
#[derive(Clone)]
pub(crate) struct FrameBatchesHold {
    _sender: Arc<watch::Sender<()>>,
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
            pop_keyboard_flags();
            let _ = execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange);
            ratatui::restore();
            // When the UI thread's terminal drops, it shows a hidden cursor
            // through its frame buffer, and a panicking thread discards that
            // buffer. After a main-thread panic, that terminal never drops.
            let _ = execute!(io::stdout(), Show);

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
    /// The executor whose clock stamps [`RenderFrame::input_time`], so the
    /// input-to-flush span ends on the clock it starts on.
    #[cfg(feature = "perf")]
    pub clock: Executor,
}

pub fn spawn(mut channels: UiChannels, mouse_captured: bool) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        rt.block_on(async move {
            let mut terminal = init_terminal().expect("failed to initialize terminal");
            // Without this a paste arrives as its characters, which normal mode
            // would run as commands. Bracketed, it arrives whole as one event.
            // The focus reports say when the terminal window loses and regains OS
            // focus.
            execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
            if mouse_captured {
                execute!(io::stdout(), EnableMouseCapture)?;
            }
            let result = run(&mut channels, &mut terminal, mouse_captured).await;
            // When an error ends the loop in the middle of a frame, the frame's
            // bytes go out here, onto the alternate screen, and not after the
            // restore below.
            let _ = terminal.backend_mut().flush();
            if mouse_captured {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
            pop_keyboard_flags();
            let _ = execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange);
            ratatui::restore();
            result
        })
    })
}

/// Raw mode, the alternate screen, and a terminal whose backend holds each
/// frame's bytes until the frame's flush.
///
/// This is what [`ratatui::try_init`] does, over [`FrameBackend`] and without
/// ratatui's own panic hook, which [`install_panic_hook`] makes redundant.
fn init_terminal() -> io::Result<Terminal<FrameBackend<Stdout>>> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    Terminal::new(FrameBackend::new(io::stdout()))
}

/// Pop the keyboard flags this session pushed, so the shell after it, or a
/// remote session it hands the terminal to, reads keys as the terminal sent
/// them before.
///
/// The panic hook, the teardown, and a hand-over call this, and the flag swap
/// makes a second call pop nothing.
fn pop_keyboard_flags() {
    if KEYBOARD_PUSHED.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
}

/// Push the flags that tell Ctrl-Shift-s from Ctrl-s and Shift-Enter from
/// Enter, and record the push for [`pop_keyboard_flags`].
fn push_keyboard_flags() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
    KEYBOARD_PUSHED.store(true, Ordering::Relaxed);
    Ok(())
}

/// The UI thread's record of when its keyboard flags belong on the terminal's
/// stack.
///
/// The terminal's answer to the protocol query says whether they go on at all.
/// A remote session reads keys with the flags off, so its ssh or mosh reads a
/// Ctrl-C as the byte that interrupts it. The flags come off when a session
/// takes the terminal and go back on when it returns. A terminal that attaches
/// brings a stack of its own, so its own answer pushes, and the return to it
/// pushes nothing.
#[derive(Default)]
struct KeyboardFlags {
    /// Whether the terminal speaks the kitty keyboard protocol.
    supported: bool,
    /// Whether a remote session owns the terminal.
    handed_over: bool,
}

impl KeyboardFlags {
    /// Records the terminal's answer and returns whether to push the flags now.
    ///
    /// An answer during a hand-over waits for the return. A stored target
    /// reconnects as soon as the terminal reports, so the hand-over sometimes
    /// reaches the UI thread ahead of the startup answer.
    fn answered(&mut self, supported: bool) -> bool {
        self.supported = supported;
        supported && !self.handed_over
    }

    /// Records that a remote session holds the terminal.
    fn hand_over(&mut self) {
        self.handed_over = true;
    }

    /// Records that the screen is back and returns whether to push the flags.
    fn resume(&mut self) -> bool {
        let push = self.handed_over && self.supported;
        self.handed_over = false;
        push
    }
}

async fn run(
    channels: &mut UiChannels,
    terminal: &mut Terminal<FrameBackend<Stdout>>,
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
        #[cfg(feature = "perf")]
        clock,
    } = channels;
    // Started before the size is read, so a resize at any later point leaves
    // its byte for the input thread, one during the handshake included.
    // crossterm never reports a resize, because every thread blocks the signal
    // and this pipe's thread takes it.
    let winch = resize_signal_pipe()?;
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
    //
    // The keyboard answer rides the channel that the input thread uses for a
    // terminal that attaches later, so the loop below pushes the flags for both.
    let (keyboard_tx, mut keyboard_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stoatty, typed, keyboard) = identify_terminal();
    let _ = stoatty_tx.send(stoatty);
    let _ = keyboard_tx.send(keyboard);
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
                    keyboard_tx: &keyboard_tx,
                    slot: &slot,
                },
                crossterm::event::poll,
                crossterm::event::read,
                tty::read_stdin,
                tty::winsize,
                identify_terminal,
                || wait_idle(slot.wake_reader(), &winch),
            )
        }
    });

    // What the terminal carries, so a frame whose squiggle is already on screen
    // writes nothing. Lives here rather than beside the paint because only this
    // thread knows which frames reached the terminal.
    let mut stamp = UndercurlStamp::default();
    // What the terminal shows, cell for cell, so a frame writes only the cells
    // it changed.
    let mut shown = ShownFrame::default();
    let mut keyboard_flags = KeyboardFlags::default();

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
                // These arms write to stdout directly, so the bytes that wait in
                // the frame buffer go out first.
                terminal.backend_mut().flush()?;
                match ctl {
                    UiControl::HandOver => {
                        keyboard_flags.hand_over();
                        pop_keyboard_flags();
                    },
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
                    // The terminal keeps one keyboard stack per screen, so the
                    // flags go back on after the alternate screen, onto the
                    // stack that the teardown pops.
                    UiControl::Resume => {
                        execute!(
                            io::stdout(),
                            EnterAlternateScreen,
                            EnableBracketedPaste,
                            EnableFocusChange
                        )?;
                        if mouse_captured {
                            execute!(io::stdout(), EnableMouseCapture)?;
                        }
                        if keyboard_flags.resume() {
                            push_keyboard_flags()?;
                        }
                        shown.repaint_all(terminal)?;
                        stamp = UndercurlStamp::default();
                    },
                }
            }

            // A terminal's answer to the keyboard protocol query, at startup or
            // after an attach. A terminal that attached holds none of the flags
            // pushed into the one that left, so that push needs no pop.
            Some(supported) = keyboard_rx.recv() => {
                terminal.backend_mut().flush()?;
                KEYBOARD_PUSHED.store(false, Ordering::Relaxed);
                if keyboard_flags.answered(supported) {
                    push_keyboard_flags()?;
                }
            }

            changed = render_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                // Open a synchronized update so the cell diff, undercurl re-stamp,
                // and APC batch below commit as one frame. Queued rather than
                // flushed, so the frame's own flush carries it ahead of the cell
                // diff in the same write.
                queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
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
                        (
                            src.buffer.clone(),
                            src.cursor,
                            src.resident.clone(),
                            src.captures.clone(),
                            src.batches.clone(),
                            restamp,
                        )
                    })
                };
                let (undercurl, batches) = match framed {
                    Some((buffer, cursor, resident, captures, batches, undercurl)) => {
                        shown.flush(terminal, &buffer, cursor, &resident, &captures)?;
                        // Drop the Arc before draining APC so the main thread's
                        // recycle can reclaim this buffer's allocation via
                        // Arc::try_unwrap instead of falling back to a fresh one.
                        drop(buffer);
                        (undercurl, Some(batches))
                    },
                    None => (None, None),
                };
                // Re-stamp diagnostic curly underlines over the grid just drawn,
                // before the APC batches composite over the same stdout.
                if let Some(bytes) = undercurl
                    && !bytes.is_empty()
                {
                    terminal.backend_mut().write_all(bytes)?;
                }
                // Write any stoatty APC byte batches the app pushed for this
                // frame to the same stdout, after the grid frame so the pool
                // composites over the content just drawn.
                drain_frame_apc(batches, FRAME_BATCH_WAIT, apc_rx, terminal.backend_mut()).await?;
                // Close the synchronized update so the cell diff, undercurl, and
                // APC batch commit as one atomic frame. Its flush sends the
                // whole frame, which the backend held until now.
                execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
                // The frame's bytes are out, so stop the input-to-flush clock.
                #[cfg(feature = "perf")]
                if let Some(started) = input_time {
                    ui_perf.record_input_to_flush(clock.now().duration_since(started));
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
                // fill commits atomically. The ESU's flush carries the batches.
                queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
                drain_apc(Some(batch), apc_rx, terminal.backend_mut())?;
                execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
            }
        }
    }

    // Final summary so a short-lived session still reports its latency.
    #[cfg(feature = "perf")]
    log_input_latency(&ui_perf);

    Ok(())
}

/// Identify the terminal that a session starts on or that attaches later.
///
/// Returns [`stoatty_handshake`]'s two answers, then whether the terminal speaks
/// the kitty keyboard protocol. Both probes read fd 0, so they run back to back
/// on the thread that owns stdin at the time.
fn identify_terminal() -> (Option<u32>, Vec<u8>, bool) {
    let (stoatty, typed) = stoatty_handshake();
    (stoatty, typed, query_keyboard())
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

/// Whether the terminal speaks the kitty keyboard protocol.
///
/// crossterm writes the query and reads the reply through its event reader, so
/// this runs only where nothing else reads stdin. A key typed meanwhile stays
/// queued for the next read. A terminal that answers neither the query nor the
/// device attributes request behind it costs the 2 seconds crossterm waits.
fn query_keyboard() -> bool {
    // crossterm writes the query to /dev/tty, past the stdout lock. Held here,
    // the lock keeps the query out of a frame that the UI thread writes at the
    // same time, such as the repaint after an attach.
    let _frame_writes = io::stdout().lock();
    let supported = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    tracing::info!(supported, "kitty keyboard protocol query");
    supported
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
    cell_pixels_from_winsize(&tty::winsize()?)
}

/// How long a raw read waits before the loop takes another turn.
///
/// The tick that polls the window size while a remote session owns fd 0, since
/// the raw read watches fd 0 alone and no resize wakes it. The idle wait needs
/// no interval of its own, since it parks on the slot's wake pipe beside fd 0.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// What ended an idle wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleWake {
    /// Something arrived on fd 0, or a signal interrupted the wait.
    Input,
    /// The app moved the slot out of idle.
    Slot,
    /// The window changed size.
    Resize,
}

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
    /// Whether the terminal answered the kitty keyboard protocol query, sent
    /// as the handshake's answer is. The query reads fd 0 too, so it runs where
    /// the handshake does.
    keyboard_tx: &'a UnboundedSender<bool>,
    /// What fd 0 feeds, which the app moves between the editor, a remote
    /// session, and a handshake.
    slot: &'a PassthroughSlot,
}

/// Forward what fd 0 produces, to the app or to the remote session the slot
/// names.
///
/// Four modes, re-read every pass, so a `:ssh` or an attach takes effect on the
/// turn after the slot moves:
///
/// - Idle: `poll` then `read` an event and send it, as a normal session does. An idle wait blocks
///   on `wait_idle` rather than spinning, so a session with nobody typing costs nothing. A resize
///   that ends the wait sends the window's size at once. crossterm reports none, since the process
///   takes SIGWINCH itself.
/// - Pending: acknowledge that fd 0 is no longer parsed, then buffer raw bytes. The app spawns ssh
///   on that ack, so anything typed in between is the user's and is handed to the session once it
///   exists.
/// - Active: write every raw byte to the session, and poll the window size, because the raw read
///   watches fd 0 alone.
/// - Handshake: identify the terminal that just attached, ask whether it speaks the kitty keyboard
///   protocol, report both, then go idle. The probe and the query read fd 0, so they belong on this
///   thread and nowhere else.
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
    mut handshake: impl FnMut() -> (Option<u32>, Vec<u8>, bool),
    mut wait_idle: impl FnMut() -> io::Result<IdleWake>,
) {
    let InputChannels {
        event_tx,
        cell_pixels_tx,
        stoatty_tx,
        keyboard_tx,
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

                // Asked first with no wait, so an event crossterm already
                // buffered is read rather than parked behind the poll below,
                // which sees only what the kernel still holds on fd 0.
                match poll(Duration::ZERO) {
                    Ok(true) => {},
                    Ok(false) => match wait_idle() {
                        // The slot moved, so the next turn takes the arm it
                        // moved to rather than reading fd 0 here.
                        Ok(IdleWake::Slot) => continue,
                        Ok(IdleWake::Input) => {},
                        Ok(IdleWake::Resize) => {
                            if let Some(ws) = winsize()
                                && !forward_event(
                                    event_tx,
                                    cell_pixels_tx,
                                    Event::Resize(ws.ws_col, ws.ws_row),
                                )
                            {
                                return;
                            }
                            continue;
                        },
                        Err(_) => return,
                    },
                    Err(_) => return,
                }
                let Ok(event) = read() else {
                    return;
                };
                if !forward_event(event_tx, cell_pixels_tx, event) {
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
                let (stoatty, typed, keyboard) = handshake();
                if stoatty_tx.send(stoatty).is_err() {
                    return;
                }
                // The terminal that attached has a keyboard stack of its own,
                // which holds none of the flags pushed into the one that left.
                if keyboard_tx.send(keyboard).is_err() {
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

/// Park until fd 0 has something, the slot leaves idle, or the window changes
/// size, however long that takes.
///
/// No timeout, so an idle session costs no wakes at all, and an arm is seen the
/// moment the app writes its byte rather than up to a poll interval later.
///
/// `winch` is the read end of [`resize_signal_pipe`]. Every thread blocks
/// SIGWINCH and that pipe's thread takes it, so only the byte it writes ends
/// the wait. Another signal that interrupts the wait reads as input.
///
/// The pipes are checked ahead of fd 0, so a wake that has one of them ready
/// reports that one whatever fd 0 holds.
fn wait_idle(wake: &PipeReader, winch: &UnixStream) -> io::Result<IdleWake> {
    let mut fds = [
        libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: winch.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];

    // SAFETY: poll reads and writes the three pollfds through the pointer and
    // touches nothing else. The array is initialized above.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(IdleWake::Input);
        }
        return Err(err);
    }

    // Each pipe is drained so a burst leaves it empty rather than waking the
    // next idle wait at once. The count does not matter, only the state.
    let mut sink = [0u8; 64];
    if fds[1].revents & libc::POLLIN != 0 {
        let _ = (&*wake).read(&mut sink);
        return Ok(IdleWake::Slot);
    }
    if fds[2].revents & libc::POLLIN != 0 {
        let _ = (&*winch).read(&mut sink);
        return Ok(IdleWake::Resize);
    }
    Ok(IdleWake::Input)
}

/// Start the thread that takes SIGWINCH, and return the read end of the socket
/// it writes one byte on per signal, for [`wait_idle`].
///
/// One thread per process, because [`spawn`] runs once. The thread feeds the
/// pipe during the handshake too, before crossterm's first read. It ends on the
/// first signal after the reader drops, when its write fails.
fn resize_signal_pipe() -> io::Result<UnixStream> {
    let (reader, mut writer) = UnixStream::pair()?;

    thread::Builder::new()
        .name("winch".to_owned())
        .spawn(move || {
            // sigwait needs the signal blocked on the calling thread. The block
            // here keeps that true for a caller of `spawn` that did not block
            // it in main.
            if tty::block_resize_signal().is_err() {
                return;
            }
            while tty::wait_resize_signal().is_ok() && writer.write_all(&[0]).is_ok() {}
        })?;

    Ok(reader)
}

/// Send `event` to the app, with the tty's cell size ahead of a resize.
///
/// A resize changes the cell count, and after a font change the cell size too.
/// The tty answers for both. The cell size goes first, so the app has it by the
/// time it lays out against the new grid.
///
/// Returns false when the app is gone.
fn forward_event(
    event_tx: &UnboundedSender<Event>,
    cell_pixels_tx: &UnboundedSender<Option<(u16, u16)>>,
    event: Event,
) -> bool {
    if matches!(event, Event::Resize(..)) {
        let _ = cell_pixels_tx.send(tty_cell_pixels());
    }
    event_tx.send(event).is_ok()
}

/// Write the APC batches of a frame whose grid `out` holds, and every other
/// batch already queued on `apc_rx`, to `out`.
///
/// The frame's batches come after the publish, from the run loop's emits and
/// from the pool jobs that the emits start, so the last of them is often still
/// on its way when the grid is written. The wait of up to `wait` for them keeps
/// them in the frame's synchronized update. Without it they go out in an update
/// of their own, and a scroll costs two writes.
async fn drain_frame_apc(
    batches: Option<FrameBatches>,
    wait: Duration,
    apc_rx: &mut UnboundedReceiver<Vec<u8>>,
    out: &mut impl Write,
) -> io::Result<()> {
    if let Some(batches) = batches {
        let _ = time::timeout(wait, batches.sent()).await;
    }
    drain_apc(None, apc_rx, out)
}

/// Write `first`, then every APC byte batch already queued on `apc_rx`, to
/// `out`.
///
/// The caller flushes `out`, so the batches leave with the frame they join.
///
/// Drains only the currently-queued batches. A batch arriving mid-drain is
/// handled on the next loop wake.
///
/// Ordered, and lossless for everything whose content this cannot reason
/// about. The exception is a pool fill superseded by a later queued one for the
/// same page, which [`supersede_stale_fills`] drops.
fn drain_apc(
    first: Option<Vec<u8>>,
    apc_rx: &mut UnboundedReceiver<Vec<u8>>,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut queued: Vec<Vec<u8>> = first.into_iter().collect();
    while let Ok(batch) = apc_rx.try_recv() {
        queued.push(batch);
    }
    if queued.is_empty() {
        return Ok(());
    }

    supersede_stale_fills(&mut queued);

    for batch in &queued {
        out.write_all(batch)?;
    }
    Ok(())
}

/// Drop each queued pool fill or row patch that a later whole fill of the same
/// page replaces, leaving everything else where it was.
///
/// A whole fill sets every row of its page's slot, and no queued command reads
/// a page's content, so an earlier fill or patch of a page that a whole fill
/// paints again later is work nobody will ever see. On a link slower than the
/// emit rate a hard scroll queues megabytes of them, and without this the user
/// watches seconds-old pages replay in order before the current one arrives.
///
/// A patch sets only its rows and keeps the rest, so it replaces nothing. The
/// fills and patches before it still paint the rows it leaves alone.
///
/// Only fills and patches are touched. Anything else is content this cannot
/// reason about, so it keeps its place and its bytes. Walking backward is what
/// makes the survivor the last whole fill of each page rather than the first.
fn supersede_stale_fills(queued: &mut Vec<Vec<u8>>) {
    let mut filled: Vec<(u32, u64)> = Vec::new();
    let mut superseded = vec![false; queued.len()];

    for (at, batch) in queued.iter().enumerate().rev() {
        let Some(key) = command::fill_batch_key(batch) else {
            continue;
        };
        let page = (key.pool, key.index);
        if filled.contains(&page) {
            superseded[at] = true;
        } else if key.whole {
            filled.push(page);
        }
    }

    let mut at = 0;
    queued.retain(|_| {
        at += 1;
        !superseded[at - 1]
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{cell::RefCell, collections::VecDeque, os::fd::RawFd, pin::pin};

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

    /// A terminal page rides a batch of its own, so a later page of its pool
    /// supersedes that batch alone, and the batch between them keeps its scroll
    /// and its window frame.
    #[test]
    fn a_superseded_terminal_page_drops_only_itself() {
        let page = |tag: &str| {
            let mut out = Vec::new();
            command::encode_fill_scope(&mut out, 3, 0, |out| out.extend_from_slice(tag.as_bytes()));
            out
        };
        let mut between = Vec::new();
        command::encode_scroll_into(
            &mut between,
            &command::ScrollCommand {
                pool: 1,
                page: 2,
                fraction: 0,
            },
        );
        command::encode_window_focus_into(&mut between, &command::WindowFocusCommand { window: 2 });

        let mut queued = vec![page("stale"), between.clone(), page("current")];
        supersede_stale_fills(&mut queued);

        assert_eq!(
            queued,
            vec![between, page("current")],
            "only the earlier page goes, and the batch between keeps its frames",
        );
    }

    /// A row patch of a page that a later whole fill paints again goes, since
    /// the fill sets every row the patch set.
    #[test]
    fn a_whole_fill_drops_earlier_patches_of_its_page() {
        let mut queued = vec![row_patch(3, 0, "rows"), page_fill(3, 0, "page")];
        supersede_stale_fills(&mut queued);

        assert_eq!(
            queued,
            vec![page_fill(3, 0, "page")],
            "the fill outlives the patch"
        );
    }

    /// A patch keeps the rows it does not set, so the fill before it still
    /// paints them and stays.
    #[test]
    fn a_patch_drops_no_fill() {
        let mut queued = vec![page_fill(3, 0, "page"), row_patch(3, 0, "rows")];
        supersede_stale_fills(&mut queued);

        assert_eq!(
            queued,
            vec![page_fill(3, 0, "page"), row_patch(3, 0, "rows")],
            "the fill and the patch both stay",
        );
    }

    /// Two patches may set different rows, so neither replaces the other.
    #[test]
    fn two_patches_of_one_page_both_stay() {
        let mut queued = vec![row_patch(3, 0, "first"), row_patch(3, 0, "second")];
        supersede_stale_fills(&mut queued);

        assert_eq!(
            queued,
            vec![row_patch(3, 0, "first"), row_patch(3, 0, "second")],
            "both patches stay, in order",
        );
    }

    /// A frame's drain waits until every hold on the frame's batches is gone, a
    /// pool job's as well as the run loop's, and then writes the batches. A hold
    /// that outlasts the wait holds the frame no longer.
    #[tokio::test]
    async fn a_frame_drains_its_batches_once_every_hold_is_gone() {
        let (apc_tx, mut apc_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut out = Vec::new();

        let (run_loop, batches) = FrameBatches::start();
        let job = run_loop.clone();
        let polls = {
            let wait = Duration::from_secs(60);
            let mut drain =
                pin!(drain_frame_apc(Some(batches), wait, &mut apc_rx, &mut out).fuse());
            let mut polls = vec![drain.as_mut().now_or_never().is_some()];
            drop(run_loop);
            polls.push(drain.as_mut().now_or_never().is_some());
            apc_tx.send(b"decorations".to_vec()).unwrap();
            drop(job);
            polls.push(drain.now_or_never().is_some_and(|drained| drained.is_ok()));
            polls
        };

        let (_stuck, batches) = FrameBatches::start();
        apc_tx.send(b"fill".to_vec()).unwrap();
        let mut expired = Vec::new();
        let wait = Duration::from_millis(1);
        time::timeout(
            Duration::from_secs(10),
            drain_frame_apc(Some(batches), wait, &mut apc_rx, &mut expired),
        )
        .await
        .expect("the wait ends at its bound")
        .unwrap();

        assert_eq!(
            (polls, out, expired),
            (
                vec![false, false, true],
                b"decorations".to_vec(),
                b"fill".to_vec()
            ),
            "the drain waits out the run loop's hold and the job's and writes the \
             job's batch, and a hold past the wait keeps back nothing queued",
        );
    }

    /// A whole fill batch for page `index` of pool `pool`, with `tag` as its
    /// page bytes.
    fn page_fill(pool: u32, index: u64, tag: &str) -> Vec<u8> {
        let mut out = Vec::new();
        command::encode_fill_scope(&mut out, pool, index, |out| {
            out.extend_from_slice(tag.as_bytes())
        });
        out
    }

    /// A one-row patch batch of page `index` of pool `pool`, with `tag` as its
    /// row bytes.
    fn row_patch(pool: u32, index: u64, tag: &str) -> Vec<u8> {
        let patch = command::FillRowsCommand {
            pool,
            index,
            first_row: 0,
            rows: 1,
            grid_rows: 6,
            grid_cols: 30,
        };
        let mut out = Vec::new();
        command::encode_fill_rows_scope(&mut out, &patch, |out| {
            out.extend_from_slice(tag.as_bytes())
        });
        out
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
        let (keyboard_tx, _keyboard_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_input(
            InputChannels {
                event_tx,
                cell_pixels_tx,
                stoatty_tx: &stoatty_tx,
                keyboard_tx: &keyboard_tx,
                slot: &slot,
            },
            |_| Ok(true),
            read,
            |_, _| Ok(0),
            || None,
            || (None, Vec::new(), false),
            || Ok(IdleWake::Input),
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
        let (keyboard_tx, _keyboard_rx) = tokio::sync::mpsc::unbounded_channel();
        forward_input(
            InputChannels {
                event_tx: &tx,
                cell_pixels_tx: &px_tx,
                stoatty_tx: &stoatty_tx,
                keyboard_tx: &keyboard_tx,
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
            || (None, Vec::new(), false),
            || Ok(IdleWake::Input),
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

    /// An idle wait parks until fd 0 has something or the slot moves, and a
    /// `:ssh` arm is what moves it. Waiting on the pipe rather than on a poll
    /// interval is what keeps an idle session free and the ack immediate.
    #[test]
    fn an_idle_wait_ends_on_the_slot_leaving_idle() {
        let armed = PassthroughSlot::new().0;
        armed.arm();
        assert_wakes(&armed, "an ssh arm");

        let attached = PassthroughSlot::new().0;
        attached.rehandshake();
        assert_wakes(&attached, "an attach");
    }

    /// Assert `slot` woke an idle wait, without risking one that never ends.
    fn assert_wakes(slot: &PassthroughSlot, what: &str) {
        let wake = slot.wake_reader();
        let (winch, _signal) = UnixStream::pair().expect("a resize pipe");
        // Checked before the blocking wait, so a transition that forgot to wake
        // fails here rather than parking the suite forever.
        assert!(readable(wake.as_raw_fd()), "{what} wrote its wake byte");
        assert_eq!(
            wait_idle(wake, &winch).expect("the wait resolves"),
            IdleWake::Slot,
            "and that byte is what ended the wait after {what}",
        );
    }

    /// Every thread blocks SIGWINCH and the resize pipe's thread takes it, so
    /// the byte that thread writes is what ends an idle wait. A burst of
    /// signals ends one wait, not one wait each.
    #[test]
    fn an_idle_wait_ends_on_the_resize_pipe_and_drains_it() {
        let (slot, _ack_rx) = PassthroughSlot::new();
        let (winch, mut signal) = UnixStream::pair().expect("a resize pipe");
        signal.write_all(&[0, 0]).expect("two signals");

        let wake = wait_idle(slot.wake_reader(), &winch).expect("the wait resolves");
        assert_eq!(
            (wake, readable(winch.as_raw_fd())),
            (IdleWake::Resize, false),
            "the resize ends the wait and leaves the pipe empty",
        );
    }

    /// Run one idle pass that a resize ends, with `arriving` the events
    /// crossterm holds once the resize wakes the pass, and return what reached
    /// the app.
    ///
    /// The zero-wait poll finds nothing, so the pass parks and the resize wakes
    /// it. The next wait ends the loop.
    fn after_a_resize_wake(arriving: Vec<Event>) -> Vec<Event> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stoatty_tx, _stoatty_rx) = tokio::sync::mpsc::unbounded_channel();
        let (keyboard_tx, _keyboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let (slot, _ack_rx) = PassthroughSlot::new();
        let held = RefCell::new(VecDeque::new());
        let mut arriving = Some(arriving);
        let eof = || io::Error::from(io::ErrorKind::UnexpectedEof);

        forward_input(
            InputChannels {
                event_tx: &tx,
                cell_pixels_tx: &px_tx,
                stoatty_tx: &stoatty_tx,
                keyboard_tx: &keyboard_tx,
                slot: &slot,
            },
            |_| Ok(!held.borrow().is_empty()),
            || held.borrow_mut().pop_front().ok_or_else(eof),
            |_, _| Ok(0),
            || {
                Some(libc::winsize {
                    ws_row: 10,
                    ws_col: 90,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                })
            },
            || (None, Vec::new(), false),
            || {
                let events = arriving.take().ok_or_else(eof)?;
                held.borrow_mut().extend(events);
                Ok(IdleWake::Resize)
            },
        );

        let mut got = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(event);
        }
        got
    }

    /// crossterm never reports a resize, since the process takes SIGWINCH
    /// itself, so a resize wake sends the window's own size. Input that
    /// crossterm already holds follows on the next turn.
    #[test]
    fn a_resize_wake_sends_the_window_size_ahead_of_held_input() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));

        assert_eq!(
            [
                after_a_resize_wake(vec![key.clone()]),
                after_a_resize_wake(Vec::new()),
            ],
            [
                vec![Event::Resize(90, 10), key],
                vec![Event::Resize(90, 10)]
            ],
            "the window's size, then a key crossterm holds, and the size alone \
             when crossterm holds nothing",
        );
    }

    /// Whether `fd` has something to read right now.
    fn readable(fd: RawFd) -> bool {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: as in `wait_idle`, with a zero timeout so it never blocks.
        unsafe { libc::poll(&raw mut poll_fd, 1, 0) > 0 }
    }

    /// The probe and the keyboard query read raw fd 0, so they belong on this
    /// thread. What was typed while the probe owned the terminal is the user's,
    /// and the app laid out against the terminal that left, so the new one's
    /// size follows.
    #[test]
    fn a_rehandshake_identifies_the_new_terminal_and_replays_what_was_typed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (px_tx, _px_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stoatty_tx, mut stoatty_rx) = tokio::sync::mpsc::unbounded_channel();
        let (keyboard_tx, mut keyboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let (slot, _ack_rx) = PassthroughSlot::new();
        slot.rehandshake();

        // The Idle pass after the handshake ends the loop, so the one probe is
        // all this runs.
        forward_input(
            InputChannels {
                event_tx: &tx,
                cell_pixels_tx: &px_tx,
                stoatty_tx: &stoatty_tx,
                keyboard_tx: &keyboard_tx,
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
            || (Some(3), b"x".to_vec(), true),
            || Ok(IdleWake::Input),
        );

        assert_eq!(
            (stoatty_rx.try_recv(), keyboard_rx.try_recv()),
            (Ok(Some(3)), Ok(true)),
            "the new terminal's protocol and its keyboard answer reach the UI",
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
                Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                Event::Resize(120, 40),
            ],
            "what was typed at the probe goes first, then the new size",
        );
    }

    /// A remote session reads keys with the flags off, and they go back on when
    /// it returns. A terminal that attached answers for itself.
    #[test]
    fn the_keyboard_flags_come_off_for_each_remote_session() {
        let mut session = KeyboardFlags::default();
        let at_startup = session.answered(true);
        session.hand_over();
        let on_return = session.resume();
        let after_an_attach = session.resume();

        let mut early = KeyboardFlags::default();
        early.hand_over();
        let during_hand_over = early.answered(true);
        let after_early_return = early.resume();

        let mut unsupported = KeyboardFlags::default();
        let refused = unsupported.answered(false);
        unsupported.hand_over();
        let refused_return = unsupported.resume();

        assert_eq!(
            [
                at_startup,
                on_return,
                after_an_attach,
                during_hand_over,
                after_early_return,
                refused,
                refused_return,
            ],
            [true, true, false, false, true, false, false],
            "a push at a yes and at each return from a remote, and none during one",
        );
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
