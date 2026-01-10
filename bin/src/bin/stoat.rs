fn main() {
    if let Err(e) = stoat::log::init() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    if let Err(e) = stoat_bin::commands::default::run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
