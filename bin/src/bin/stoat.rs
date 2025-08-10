use clap::Parser;
use stoat::cli::config::{Cli, Command};
use stoat_core::{Stoat, StoatConfig};

fn main() {
    let cli = Cli::parse();

    // Initialize logging
    stoat_core::log::init().expect("failed to init logs");

    // Handle subcommands
    match cli.command {
        #[cfg(feature = "gui")]
        Some(Command::Gui) => {
            // Launch GUI directly without any tokio runtime
            if let Err(e) = stoat_bin::commands::gui::run() {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        None => {
            // For CLI commands that need async, create a runtime
            // For now, this is synchronous since no async operations are needed
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

            // When async operations are needed in the future, create runtime like this:
            // let runtime = tokio::runtime::Runtime::new().unwrap();
            // runtime.block_on(async {
            //     // async operations here
            // });
        },
    }
}
