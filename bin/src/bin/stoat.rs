fn main() {
    let stoat_log = std::env::var("STOAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    if let Err(e) = stoat::log::init(stoat_log, rust_log) {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    if let Err(e) = stoat_bin::commands::default::run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
