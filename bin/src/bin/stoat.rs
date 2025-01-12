use clap::Parser;
use stoat::cli::config::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let Command::Gui = cli.command;
    stoat_gui_bevy::main()
}
