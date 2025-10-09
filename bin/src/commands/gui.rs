use std::path::PathBuf;

#[cfg(feature = "gui")]
pub fn run(paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    stoat_gui::run_with_paths(paths)
}

#[cfg(not(feature = "gui"))]
pub fn run(_paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Error: gui feature not enabled");
    eprintln!("Run with: cargo run --features gui --bin stoat -- gui");
    std::process::exit(1);
}
