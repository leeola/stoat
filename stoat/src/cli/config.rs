use super::csv::*;
// Future command imports (commented out with their respective commands):
// use super::{link::*, node::*, run::*, workspace::*};
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
    #[arg(long, env, global = true)]
    pub state_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    // /// Manage workspaces
    // #[command(subcommand)]
    // Workspace(WorkspaceCommand),

    // /// Manage nodes in the workspace
    // #[command(subcommand)]
    // Node(NodeCommand),

    // /// Create links between nodes
    // Link(LinkArgs),

    // /// Run workspace or nodes
    // Run(RunArgs),
    /// Quick CSV operations
    #[command(subcommand)]
    Csv(CsvCommand),
    // /// Show workspace status
    // Status(StatusArgs),

    // /// Interactive REPL mode
    // Repl,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Yaml,
    Plain,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Show execution history
    #[arg(long)]
    pub history: bool,

    /// Number of history entries
    #[arg(long, default_value = "10")]
    pub last: usize,

    /// Show node details
    #[arg(long)]
    pub nodes: bool,

    /// Show link details
    #[arg(long)]
    pub links: bool,
}
