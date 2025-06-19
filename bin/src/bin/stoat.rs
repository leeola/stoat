use clap::Parser;
use stoat::cli::config::{Cli, Command};
use stoat_bin::commands;
use stoat_core::{Stoat, StoatConfig};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize Stoat with configuration from CLI
    let config = StoatConfig {
        state_dir: cli.state_dir,
        workspace: cli.workspace,
    };

    let stoat = Stoat::new_with_config(config).unwrap_or_else(|e| {
        eprintln!("Error: Failed to initialize Stoat: {}", e);
        std::process::exit(1);
    });

    // Execute command
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Command::Node(node_cmd) => commands::node::handle(node_cmd, &stoat),
    };

    // Handle command result and save state
    if let Err(e) = result {
        eprintln!("Command failed: {}", e);
        std::process::exit(1);
    }

    // Save state and workspace
    if let Err(e) = stoat.save() {
        eprintln!("Warning: Failed to save: {}", e);
    }
}

