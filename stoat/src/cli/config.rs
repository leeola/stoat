use super::node::*;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "stoat")]
#[command(about = "A node-based text editor and data pipeline tool")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Workspace to use (overrides current)
    #[arg(short, long, global = true)]
    pub workspace: Option<String>,

    /// Directory for storing state files (overrides default)
    #[arg(long = "stoat-dir", env = "STOAT_DIR", global = true)]
    pub state_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage nodes in the workspace
    #[command(subcommand)]
    Node(NodeCommand),
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Yaml,
    Plain,
}
