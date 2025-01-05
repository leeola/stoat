use clap::Parser;

#[derive(Debug, Parser)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
pub enum Command {
    #[cfg(feature = "gui")]
    Gui,
}
