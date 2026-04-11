use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};
use stoat::{Axis, Settings, Stoat};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_scheduler::TestScheduler;

#[derive(Parser)]
#[command(name = "stoat", about = "A modal text editor")]
pub struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(help = "Files to open")]
    pub files: Vec<PathBuf>,

    /// Enable the Claude Code / LSP text-protocol transcript log. Overrides
    /// the stcfg `text_proto_log` setting when set.
    #[arg(long, env = "STOAT_TEXT_PROTO_LOG")]
    pub text_proto_log: Option<bool>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the first changed file with a diff against HEAD
    Review,
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

    let cli_settings = Settings {
        text_proto_log: args.text_proto_log,
    };

    rt.block_on(async {
        let mut stoat = Stoat::new(executor, cli_settings);
        stoat.set_claude_code_factory(Arc::new(ClaudeCodeLauncher::new()));

        match args.command {
            Some(Command::Review) => stoat.open_review(),
            None => {
                for (i, path) in args.files.iter().enumerate() {
                    if i > 0 {
                        stoat.panes.split(Axis::Vertical);
                    }
                    stoat.open_file(path);
                }
            },
        }

        stoat.run(event_rx, render_tx).await
    })?;

    ui_handle.join().expect("ui thread panicked")?;

    Ok(())
}
