pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat TUI...");
    Ok(stoat::run()?)
}
