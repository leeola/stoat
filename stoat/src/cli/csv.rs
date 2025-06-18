use crate::cli::config::OutputFormat;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum CsvCommand {
    /// Load and preview CSV
    Load {
        /// Path to CSV file
        path: PathBuf,

        /// Number of rows to preview
        #[arg(short, long, default_value = "10")]
        rows: usize,

        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,
    },

    /// Filter CSV data
    Filter {
        /// Path to CSV file
        path: PathBuf,

        /// Filter expression
        expression: String,

        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,

        /// Save to file
        #[arg(long)]
        save: Option<PathBuf>,
    },

    /// Sort CSV data
    Sort {
        /// Path to CSV file
        path: PathBuf,

        /// Sort specification (column:direction)
        spec: String,

        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,

        /// Save to file
        #[arg(long)]
        save: Option<PathBuf>,
    },

    /// Query CSV with multiple operations
    Query {
        /// Path to CSV file
        path: PathBuf,

        /// Filter expression
        filter: Option<String>,

        /// Sort specification
        #[arg(long)]
        sort: Option<String>,

        /// Limit rows
        #[arg(long)]
        limit: Option<usize>,

        /// Select specific columns
        #[arg(long)]
        select: Option<Vec<String>>,

        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,

        /// Save to file
        #[arg(long)]
        save: Option<PathBuf>,
    },
}
