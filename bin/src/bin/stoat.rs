use clap::Parser;

fn main() {
    let args = stoat_bin::commands::default::Args::parse();
    let stoat_log = std::env::var("STOAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let target = if args.log_stderr {
        stoat::log::LogTarget::Stderr
    } else {
        match resolve_log_path() {
            Ok(p) => stoat::log::LogTarget::File(p),
            Err(e) => {
                eprintln!("Failed to prepare log directory: {e}");
                std::process::exit(1);
            },
        }
    };
    if let Err(e) = stoat::log::init(stoat_log, rust_log, target) {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    tracing::info!("Starting Stoat editor");

    if let Err(e) = stoat_bin::commands::default::run(args) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn resolve_log_path() -> std::io::Result<std::path::PathBuf> {
    let dir = stoat::log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("stoat-{}.log", std::process::id())))
}
