use crate::cli::config::OutputFormat;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Specific workspace to run (uses current if not specified)
    pub workspace: Option<String>,

    /// Execution target
    #[command(subcommand)]
    pub target: Option<RunTarget>,

    /// Output format
    #[arg(short, long, value_enum, default_value = "table")]
    pub output: OutputFormat,

    /// Save output to file
    #[arg(long)]
    pub save: Option<PathBuf>,

    /// Override input paths (format: node=path)
    #[arg(long = "input")]
    pub inputs: Vec<String>,

    /// Override parameters (format: key=value)
    #[arg(long = "param")]
    pub params: Vec<String>,

    /// Watch for input changes and re-run
    #[arg(long)]
    pub watch: bool,

    /// Run on schedule (cron expression)
    #[arg(long, conflicts_with = "watch")]
    pub schedule: Option<String>,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Show detailed execution information
    #[arg(short, long)]
    pub verbose: bool,

    /// Dry run - show what would be executed
    #[arg(long)]
    pub dry_run: bool,

    /// Run with limited sample data
    #[arg(long)]
    pub sample: Option<usize>,

    /// Show performance profiling
    #[arg(long)]
    pub profile: bool,

    /// Debug specific nodes
    #[arg(long)]
    pub debug: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum RunTarget {
    /// Run specific node
    Node {
        /// Node ID or name
        id: String,
    },

    /// Run until specific node
    Until {
        /// Node ID or name
        id: String,
    },

    /// Run from specific node
    From {
        /// Node ID or name
        id: String,
    },

    /// Run specific named output
    Output {
        /// Output name
        name: String,
    },
}
