use clap::Parser;

#[derive(Parser)]
#[command(name = "stoat")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Parser)]
enum Command {
    #[command(about = "Launch GUI")]
    Gui,
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = stoat::log::init() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    match cli.command {
        Some(Command::Gui) | None => {
            if let Err(e) = stoat_bin::commands::gui::run() {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
    }
}
