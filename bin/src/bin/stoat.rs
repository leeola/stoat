use clap::Parser;
use stoat::cli::config::Cli;

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
}
