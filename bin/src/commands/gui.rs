pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat GUI...");
    stoat::run()
}
