use clap::Parser;

#[derive(Parser)]
#[command(name = "stoat")]
#[command(about = "A text editor", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Parser)]
pub enum Command {
    #[command(about = "Launch GUI with v4 architecture", name = "gui")]
    Gui {
        #[arg(help = "Files to open")]
        paths: Vec<std::path::PathBuf>,
    },
}

fn main() {
    // Initialize logging with STOAT_LOG support
    if let Err(e) = stoat::log::init() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    let cli = Cli::parse();

    // Handle subcommands
    match cli.command {
        Some(Command::Gui { paths }) => {
            // Launch GUI
            if let Err(e) = stoat_bin::commands::gui::run(paths) {
                eprintln!("Error: Failed to launch GUI: {e}");
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("Stoat editor. Use 'stoat gui' to launch the graphical interface.");
        },
    }
}
