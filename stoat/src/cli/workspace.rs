use crate::cli::config::OutputFormat;
use clap::{Subcommand, ValueEnum};

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Create a new workspace
    New {
        /// Workspace name
        name: String,

        /// Initial description
        #[arg(short, long)]
        description: Option<String>,
    },

    /// List all workspaces
    List {
        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,
    },

    /// Load a workspace
    Load {
        /// Workspace name
        name: String,
    },

    /// Save current workspace
    Save {
        /// Save with a new name
        #[arg(short, long)]
        as_name: Option<String>,
    },

    /// Configure workspace settings
    #[command(subcommand)]
    Config(WorkspaceConfigCommand),

    /// Manage workspace outputs
    #[command(subcommand)]
    Output(WorkspaceOutputCommand),
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceConfigCommand {
    /// Set a configuration value
    Set {
        /// Configuration key
        key: ConfigKey,

        /// Configuration value
        value: String,
    },

    /// Get a configuration value
    Get {
        /// Configuration key
        key: ConfigKey,
    },

    /// List all configurations
    List,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum ConfigKey {
    Description,
    DefaultOutput,
    AutoInputs,
    Params,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceOutputCommand {
    /// Add a named output
    Add {
        /// Output name
        name: String,

        /// Node ID or name
        #[arg(long)]
        node: String,

        /// Output format
        #[arg(long, value_enum)]
        format: OutputFormat,

        /// Optional condition for output
        #[arg(long)]
        condition: Option<String>,
    },

    /// List named outputs
    List,

    /// Remove a named output
    Remove {
        /// Output name
        name: String,
    },
}
