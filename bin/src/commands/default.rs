use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use stoat::{Axis, Stoat};
use stoat_scheduler::TestScheduler;

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[arg(help = "Files to open")]
    pub files: Vec<PathBuf>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    // Capacity 1: natural backpressure -- main thread won't render ahead
    // if the UI thread hasn't flushed the previous frame yet
    let (render_tx, render_rx) = tokio::sync::mpsc::channel(1);

    let ui_handle = stoat::ui::spawn(event_tx, render_rx);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // FIXME: Replace TestScheduler with a production scheduler
    let scheduler = Arc::new(TestScheduler::new());
    let executor = scheduler.executor();

    rt.block_on(async {
        let mut stoat = Stoat::new(executor);

        for (i, path) in args.files.iter().enumerate() {
            if i > 0 {
                stoat.panes.split(Axis::Vertical);
            }
            stoat.open_file(path);
        }

        stoat.run(event_rx, render_tx).await
    })?;

    ui_handle.join().expect("ui thread panicked")?;

    Ok(())
}
