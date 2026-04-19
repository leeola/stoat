use clap::{ArgAction, Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};
use stoat::{Axis, Settings, Stoat};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_scheduler::TestScheduler;

const VERSION_INFO: &str = concat!(
    env!("STOAT_GIT_HASH"),
    " (",
    env!("STOAT_GIT_DIRTY"),
    ")\n  built: ",
    env!("STOAT_BUILD_DATE"),
    "\n  commit: ",
    env!("STOAT_GIT_TITLE"),
);

#[derive(Parser)]
#[command(
    name = "stoat",
    about = "A modal text editor",
    version = VERSION_INFO,
    disable_version_flag = true,
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(help = "Files to open")]
    pub files: Vec<PathBuf>,

    /// Enable the Claude Code / LSP text-protocol transcript log. Overrides
    /// the stcfg `text_proto_log` setting when set.
    #[arg(long, env = "STOAT_TEXT_PROTO_LOG")]
    pub text_proto_log: Option<bool>,

    #[arg(
        short = 'v',
        long = "version",
        action = ArgAction::Version,
        help = "Print version info",
    )]
    _version: Option<bool>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the first changed file with a diff against HEAD
    Review,
    /// Manage workspace dumps (captured tarballs of the repo + stoat state).
    Dump {
        #[command(subcommand)]
        sub: crate::commands::dump::DumpCommand,
    },
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        command,
        files,
        text_proto_log,
        ..
    } = Args::parse();

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Review) => run_tui(text_proto_log, files, TuiStart::Review),
        None => run_tui(text_proto_log, files, TuiStart::Files),
    }
}

enum TuiStart {
    Review,
    Files,
}

fn run_tui(
    text_proto_log: Option<bool>,
    files: Vec<PathBuf>,
    start: TuiStart,
) -> Result<(), Box<dyn std::error::Error>> {
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
        text_proto_log,
        claude_default_placement: None,
    };

    let initial_git_root = std::env::current_dir().unwrap_or_default();

    rt.block_on(async {
        let mut stoat = Stoat::new(executor, cli_settings, initial_git_root);
        stoat.set_claude_code_host(Arc::new(ClaudeCodeLauncher::new()));

        match start {
            TuiStart::Review => stoat.open_review(),
            TuiStart::Files => {
                for (i, path) in files.iter().enumerate() {
                    if i > 0 {
                        stoat.active_workspace_mut().panes.split(Axis::Vertical);
                    }
                    stoat.open_file(path);
                }
            },
        }

        if let Ok(raw) = std::env::var("STOAT_DUMP_LOAD") {
            let dump_path = PathBuf::from(&raw);
            if dump_path.exists() {
                match stoat::dump::hydrate(&mut stoat, &dump_path) {
                    Ok(()) => {
                        tracing::info!(path = %raw, "hydrated workspace from dump");
                    },
                    Err(e) => {
                        tracing::error!(error = %e, path = %raw, "failed to hydrate dump");
                    },
                }
            } else {
                tracing::warn!(path = %raw, "STOAT_DUMP_LOAD points at missing file");
            }
        }

        stoat.run(event_rx, render_tx).await
    })?;

    ui_handle.join().expect("ui thread panicked")?;

    Ok(())
}
