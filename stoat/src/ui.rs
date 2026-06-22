//! UI thread: owns the terminal and stdin event stream.
//!
//! Runs on a dedicated OS thread with its own single-threaded tokio runtime.
//! Its only job is shuttling bytes -- forwarding input events to the main thread
//! and flushing rendered buffers to the terminal. Physical thread isolation
//! guarantees that terminal IO latency is independent of main-thread workload.

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
};
use futures::StreamExt;
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    backtrace::Backtrace,
    io::{self, Write},
    panic,
    sync::Once,
    thread,
};
use tokio::sync::{
    mpsc::{Sender, UnboundedReceiver},
    watch,
};

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
    mut render_rx: watch::Receiver<Option<Buffer>>,
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
    render_rx: &mut watch::Receiver<Option<Buffer>>,
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

    let mut events = EventStream::new();

    // Reused so a redraw copies into this buffer's allocation instead of
    // cloning a fresh one out of the watch each frame.
    let mut frame = Buffer::empty(Rect::new(0, 0, size.width, size.height));

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
                // Copy the latest frame into `frame` and drop the watch borrow
                // before drawing, so the slow terminal flush never holds the
                // lock the render thread needs to publish the next frame.
                let has_frame = {
                    let latest = render_rx.borrow_and_update();
                    match latest.as_ref() {
                        Some(src) => {
                            frame.resize(src.area);
                            frame.content.clone_from(&src.content);
                            true
                        },
                        None => false,
                    }
                };
                if has_frame {
                    terminal.draw(|f| {
                        let dst = f.buffer_mut();
                        dst.resize(frame.area);
                        dst.content.clone_from(&frame.content);
                    })?;
                }
                // Write any stoatty APC byte batches the app pushed for this
                // frame to the same stdout, after the grid frame so the pool
                // composites over the content just drawn.
                drain_apc(apc_rx)?;
            }

            batch = apc_rx.recv() => {
                let Some(batch) = batch else { break };
                let mut stdout = io::stdout();
                stdout.write_all(&batch)?;
                drain_apc(apc_rx)?;
                stdout.flush()?;
            }
        }
    }

    Ok(())
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
