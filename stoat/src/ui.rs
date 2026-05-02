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
use std::{env, io, thread};
use stoat_config::MouseCapturePolicy;
use tokio::sync::mpsc::{Receiver, Sender};

pub fn spawn(
    event_tx: Sender<Event>,
    mut render_rx: Receiver<Buffer>,
    mouse_capture: MouseCapturePolicy,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        rt.block_on(async move {
            let mut terminal = ratatui::init();
            let mouse_captured = match mouse_capture {
                MouseCapturePolicy::Always => {
                    execute!(io::stdout(), EnableMouseCapture)?;
                    true
                },
                MouseCapturePolicy::Never => false,
                MouseCapturePolicy::Auto => {
                    // FIXME: route through EnvHost once it exists. Parent
                    // multiplexers (tmux, zellij) own the mouse drag-select
                    // gesture; capturing here would steal their events,
                    // breaking the user's expected workflow.
                    let inside_mux =
                        env::var_os("TMUX").is_some() || env::var_os("ZELLIJ").is_some();
                    if inside_mux {
                        false
                    } else {
                        execute!(io::stdout(), EnableMouseCapture)?;
                        true
                    }
                },
            };
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
