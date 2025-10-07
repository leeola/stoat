use std::path::PathBuf;

#[cfg(feature = "gui_v4")]
pub fn run(paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    stoat_gui_v4::run_with_paths(paths)
}

#[cfg(not(feature = "gui_v4"))]
pub fn run(_paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Error: gui_v4 feature not enabled");
    eprintln!("Run with: cargo run --features gui_v4 --bin stoat -- gui-v4");
    std::process::exit(1);
}
