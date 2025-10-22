use std::path::PathBuf;

#[cfg(feature = "gui")]
pub fn run(
    config_path: Option<PathBuf>,
    paths: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    stoat::app::run_with_paths(config_path, paths)
}

#[cfg(not(feature = "gui"))]
pub fn run(
    _config_path: Option<PathBuf>,
    _paths: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Error: gui feature not enabled");
    eprintln!("Run with: cargo run --features gui --bin stoat -- gui");
    std::process::exit(1);
}
