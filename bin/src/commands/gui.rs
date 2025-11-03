use std::path::PathBuf;

#[cfg(feature = "gui")]
pub fn run(
    config_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    input: Option<String>,
    #[cfg(debug_assertions)] timeout: Option<u64>,
    paths: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(cwd) = cwd {
        std::env::set_current_dir(&cwd)
            .map_err(|e| format!("Failed to change to directory '{}': {}", cwd.display(), e))?;
    }
    #[cfg(debug_assertions)]
    {
        stoat::app::run_with_paths(config_path, input, timeout, paths)
    }
    #[cfg(not(debug_assertions))]
    {
        stoat::app::run_with_paths(config_path, input, paths)
    }
}

#[cfg(not(feature = "gui"))]
pub fn run(
    _config_path: Option<PathBuf>,
    _cwd: Option<PathBuf>,
    _input: Option<String>,
    #[cfg(debug_assertions)] _timeout: Option<u64>,
    _paths: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Error: gui feature not enabled");
    eprintln!("Run with: cargo run --features gui --bin stoat -- gui");
    std::process::exit(1);
}
