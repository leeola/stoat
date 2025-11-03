use clap::Parser;

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

        #[arg(long, help = "Set log level (info, debug, trace)")]
        log: Option<String>,

        #[cfg(debug_assertions)]
        #[arg(long, help = "Auto-quit after N seconds (dev builds only)")]
        timeout: Option<u64>,

        #[arg(help = "Files to open")]
        paths: Vec<std::path::PathBuf>,
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
            log: _,
            #[cfg(debug_assertions)]
            timeout,
            paths,
        }) => {
            // Launch GUI
            #[cfg(debug_assertions)]
            let result = stoat_bin::commands::gui::run(config, cwd, timeout, paths);
            #[cfg(not(debug_assertions))]
            let result = stoat_bin::commands::gui::run(config, cwd, paths);

            if let Err(e) = result {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("Stoat editor. Use 'stoat gui' to launch the graphical interface.");
        },
    }
}
