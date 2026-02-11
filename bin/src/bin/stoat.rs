use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "stoat")]
#[command(about = "A text editor", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Parser)]
pub enum Command {
    #[command(about = "Launch GUI with v4 architecture", name = "gui")]
    Gui {
        #[arg(short, long, env = "STOAT_CONFIG", help = "Path to config file")]
        config: Option<std::path::PathBuf>,

        #[arg(long, help = "Set working directory at startup")]
        cwd: Option<std::path::PathBuf>,

        #[arg(
            long,
            help = "Simulate keystroke input for testing/debugging (e.g., ':cd foo<Enter>')"
        )]
        input: Option<String>,

        #[arg(long, help = "Set log level (info, debug, trace)")]
        log: Option<String>,

        #[cfg(debug_assertions)]
        #[arg(long, help = "Auto-quit after N seconds (dev builds only)")]
        timeout: Option<u64>,

        #[arg(help = "Files to open")]
        paths: Vec<std::path::PathBuf>,
    },

    #[cfg(feature = "dev-tools")]
    #[command(about = "Development tools", name = "dev-tools")]
    DevTools {
        #[command(subcommand)]
        sub: DevToolsCommand,
    },
}

#[cfg(feature = "dev-tools")]
#[derive(Parser)]
pub enum DevToolsCommand {
    #[command(about = "Git test fixtures")]
    Git {
        #[command(subcommand)]
        action: GitAction,
    },
}

#[cfg(feature = "dev-tools")]
#[derive(Parser)]
pub enum GitAction {
    #[command(about = "List available scenarios")]
    List,
    #[command(about = "Open a scenario in the editor")]
    Open {
        scenario: String,
        #[arg(
            long,
            env = "STOAT_DEV_TEMP_DIR",
            help = "Base directory for fixture repos"
        )]
        base_temp_dir: Option<PathBuf>,
        #[arg(long, help = "Keep the fixture directory after exit")]
        persist: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // Set STOAT_LOG env var if --log flag was provided
    if let Some(Command::Gui {
        log: Some(ref log_level),
        ..
    }) = cli.command
    {
        std::env::set_var("STOAT_LOG", log_level);
    }

    // Initialize logging with STOAT_LOG support
    if let Err(e) = stoat::log::init() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    let build_info = stoat::build_info::build_info();
    tracing::info!(
        commit = build_info.commit_hash,
        dirty = build_info.dirty,
        "Build information"
    );

    // Handle subcommands
    match cli.command {
        Some(Command::Gui {
            config,
            cwd,
            input,
            log: _,
            #[cfg(debug_assertions)]
            timeout,
            paths,
        }) => {
            // Launch GUI
            #[cfg(debug_assertions)]
            let result = stoat_bin::commands::gui::run(config, cwd, input, timeout, paths);
            #[cfg(not(debug_assertions))]
            let result = stoat_bin::commands::gui::run(config, cwd, input, paths);

            if let Err(e) = result {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        #[cfg(feature = "dev-tools")]
        Some(Command::DevTools { sub }) => {
            use DevToolsCommand::*;
            use GitAction::*;
            let result = match sub {
                Git { action: List } => stoat_bin::commands::dev_tools::run_git_list(),
                Git {
                    action:
                        Open {
                            scenario,
                            base_temp_dir,
                            persist,
                        },
                } => stoat_bin::commands::dev_tools::run_git_open(
                    &scenario,
                    base_temp_dir.as_deref(),
                    persist,
                ),
            };
            if let Err(e) = result {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("Stoat editor. Use 'stoat gui' to launch the graphical interface.");
        },
    }
}
