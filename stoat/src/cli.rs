pub mod config {
    use clap::Parser;

    /// Command-line interface configuration
    #[derive(Parser)]
    #[command(author, version, about, long_about = None)]
    pub struct Cli {
        /// Subcommand to run
        #[command(subcommand)]
        pub command: Option<Command>,
    }

    /// Available CLI commands
    #[derive(clap::Subcommand)]
    pub enum Command {
        /// Launch GUI mode
        Gui {
            /// Optional input sequence for automated testing
            #[arg(short, long)]
            input: Option<String>,
        },
    }
}
