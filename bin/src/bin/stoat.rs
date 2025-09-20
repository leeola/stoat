use clap::Parser;
use stoat::cli::config::{Cli, Command};

fn main() {
    // Initialize logging as early as possible
    if let Err(e) = stoat::log::init() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    let cli = Cli::parse();

    // Handle subcommands
    match cli.command {
        #[cfg(feature = "gui")]
        Some(Command::Gui { paths, input }) => {
            // Create a Stoat instance and load files if provided
            let mut stoat = stoat::Stoat::new();
            if !paths.is_empty() {
                // Convert PathBuf to Path references
                let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_ref()).collect();
                stoat.load_files(&path_refs);
            }

            // Launch GUI directly without any tokio runtime
            if let Err(e) = stoat_bin::commands::gui::run(stoat, input) {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        None => {
            // For CLI mode, just create a simple editor engine
            let _engine = stoat::EditorEngine::new();

            // No commands implemented yet - just print a message
            eprintln!(
                "Stoat editor initialized. Use 'stoat gui' to launch the graphical interface."
            );
        },
    }
}
