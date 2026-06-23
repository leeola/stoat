//! Binary entry point for the `stoatty` terminal: parses argv, opens a window
//! running the requested command (or the user's shell), and drives the event
//! loop until the window closes.

use clap::Parser;

fn main() {
    let cli = stoatty::cli::Cli::parse();
    stoatty::app::run(cli.command(), cli.working_directory);
}
