use clap::Parser;
use stoat::cli::config::{Cli, Command};
use stoat_core::{Stoat, StoatConfig};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize logging
    stoat_core::log::init().expect("failed to init logs");

    // Handle subcommands
    match cli.command {
        #[cfg(feature = "gui")]
        Some(Command::Gui) => {
            // Launch GUI without initializing Stoat core
            if let Err(e) = stoat_bin::commands::gui::run() {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        None => {
            // Default behavior: initialize Stoat CLI
            let config = StoatConfig {
                state_dir: cli.state_dir,
                workspace: cli.workspace,
            };

            let stoat = Stoat::new_with_config(config).unwrap_or_else(|e| {
                eprintln!("Error: Failed to initialize Stoat: {e}");
                std::process::exit(1);
            });

            // No commands implemented yet - just print a message
            eprintln!("Stoat editor initialized. No commands available yet.");

            // Save state and workspace
            if let Err(e) = stoat.save() {
                eprintln!("Warning: Failed to save: {e}");
            }
        },
    }
}
