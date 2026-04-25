use clap::{ArgAction, Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};
use stoat::{host::LocalFs, Axis, Settings, Stoat};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_scheduler::TestScheduler;

const VERSION_INFO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("STOAT_BUILD_INFO"),
    ")",
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

    /// Restore the most-recently-used workspace for this repository instead
    /// of starting in a fresh one. `continue` is a Rust keyword so the field
    /// is named `continue_`; clap exposes it as `--continue` / `-c`.
    #[arg(short = 'c', long = "continue")]
    pub continue_: bool,

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
        continue_,
        text_proto_log,
        ..
    } = Args::parse();

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Review) => run_tui(text_proto_log, files, continue_, TuiStart::Review),
        None => run_tui(text_proto_log, files, continue_, TuiStart::Files),
    }
}

enum TuiStart {
    Review,
    Files,
}

fn run_tui(
    text_proto_log: Option<bool>,
    files: Vec<PathBuf>,
    continue_: bool,
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
        theme: None,
    };

    let initial_git_root = std::env::current_dir().unwrap_or_default();

    rt.block_on(async {
        let mut stoat = Stoat::new(executor, cli_settings, initial_git_root);
        if continue_ {
            stoat.load_active_workspace_state();
        }
        stoat.set_claude_code_host(Arc::new(ClaudeCodeLauncher::new(Arc::new(LocalFs))));

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
                match stoat::dump::hydrate(&mut stoat, &dump_path, &LocalFs) {
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
