use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "stoat")]
#[command(about = "A node-based text editor")]
#[command(version)]
pub struct Cli {
    /// Workspace to use (overrides current)
    #[arg(short, long, global = true)]
    pub workspace: Option<String>,

    /// Directory for storing state files (overrides default)
    #[arg(long = "stoat-dir", env = "STOAT_DIR", global = true)]
    pub state_dir: Option<std::path::PathBuf>,
}
