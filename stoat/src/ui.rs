//! UI thread: owns the terminal and stdin event stream.
//!
//! Runs on a dedicated OS thread with its own single-threaded tokio runtime.
//! Its only job is shuttling bytes -- forwarding input events to the main thread
//! and flushing rendered buffers to the terminal. Physical thread isolation
//! guarantees that terminal IO latency is independent of main-thread workload.

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::buffer::Buffer;
use std::{io, thread};
use tokio::sync::mpsc::{Receiver, Sender};

pub fn spawn(
    event_tx: Sender<Event>,
    mut render_rx: Receiver<Buffer>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        rt.block_on(async move {
            let mut terminal = ratatui::init();
            let result = run(&event_tx, &mut render_rx, &mut terminal).await;
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
