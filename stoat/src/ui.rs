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
use ratatui::buffer::Buffer;
use std::{backtrace::Backtrace, io, panic, sync::Once, thread};
use tokio::sync::mpsc::{Receiver, Sender};

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
    mut render_rx: Receiver<Buffer>,
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
            let result = run(&event_tx, &mut render_rx, &mut terminal).await;
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
    render_rx: &mut Receiver<Buffer>,
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

            buf = render_rx.recv() => {
                let Some(buf) = buf else { break };
                terminal.draw(|f| *f.buffer_mut() = buf)?;
            }
        }
    }

    Ok(())
}
