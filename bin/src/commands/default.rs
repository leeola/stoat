use clap::Parser;
use std::sync::Arc;
use stoat::Stoat;
use stoat_scheduler::TestScheduler;

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[arg(help = "File to open")]
    pub file: Option<std::path::PathBuf>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _args = Args::parse();

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
        stoat.run(event_rx, render_tx).await
    })?;

    ui_handle.join().expect("ui thread panicked")?;

    Ok(())
}
