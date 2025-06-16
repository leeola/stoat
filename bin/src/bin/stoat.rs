use clap::Parser;
use stoat::{
    cli::config::{Cli, Command},
    Stoat,
};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let Command::Gui = cli.command;
    let stoat = Stoat::new();
    stoat.load_state().unwrap();
}
