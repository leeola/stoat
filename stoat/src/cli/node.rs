use crate::cli::config::OutputFormat;
use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Add a new node
    #[command(subcommand)]
    Add(AddNodeCommand),

    /// List all nodes
    List {
        /// Show detailed information
        #[arg(short, long)]
        detailed: bool,

        /// Filter by type
        #[arg(short, long)]
        type_filter: Option<NodeTypeFilter>,
    },

    /// Show node details
    Show {
        /// Node ID or name
        node: String,

        /// Show outputs
        #[arg(long)]
        outputs: bool,
    },

    /// Execute a specific node
    Execute {
        /// Node ID or name
        node: String,

        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,

        /// Save output to file
        #[arg(long)]
        save: Option<PathBuf>,
    },

    /// Remove a node
    Remove {
        /// Node ID or name
        node: String,

        /// Force removal (even if linked)
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AddNodeCommand {
    /// Add CSV data source
    Csv {
        /// Path to CSV file
        path: PathBuf,

        /// Node name
        #[arg(short, long)]
        name: Option<String>,

        /// CSV delimiter
        #[arg(long, default_value = ",")]
        delimiter: char,

        /// Has headers
        #[arg(long, default_value = "true")]
        headers: bool,
    },

    /// Add table viewer node
    Table {
        /// Node name
        #[arg(short, long)]
        name: Option<String>,

        /// Maximum rows to display
        #[arg(long)]
        max_rows: Option<usize>,
    },

    /// Add JSON data source
    Json {
        /// Path to JSON file
        path: PathBuf,

        /// Node name
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Add API data source
    Api {
        /// API endpoint URL
        url: String,

        /// Node name
        #[arg(short, long)]
        name: Option<String>,

        /// HTTP method
        #[arg(long, value_enum, default_value = "get")]
        method: HttpMethod,

        /// Headers in key=value format
        #[arg(long)]
        headers: Vec<String>,
    },
}

#[derive(Debug, Clone, ValueEnum)]
pub enum NodeTypeFilter {
    Csv,
    Json,
    Table,
    Api,
    Transform,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}
