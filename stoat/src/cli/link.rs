use clap::Args;

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// Source in format node:port
    pub from: String,

    /// Target in format node:port
    pub to: String,

    /// Filter expression (e.g., "name=Alice")
    #[arg(long)]
    pub filter: Option<String>,

    /// Sort specification (e.g., "age:desc")
    #[arg(long)]
    pub sort: Option<String>,

    /// Limit number of records
    #[arg(long)]
    pub limit: Option<usize>,

    /// Chain multiple transformations (e.g., "filter:name=Alice|sort:age:desc")
    #[arg(long, conflicts_with_all = ["filter", "sort", "limit"])]
    pub chain: Option<String>,

    /// Remove existing link first
    #[arg(long)]
    pub replace: bool,
}
