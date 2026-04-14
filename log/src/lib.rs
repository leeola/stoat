//! Logging setup for Stoat.
//!
//! ## Usage
//!
//! ```bash
//! # Default: quiet, only important messages
//! stoat gui
//!
//! # Debug level for all stoat crates
//! STOAT_LOG=debug stoat gui
//!
//! # Trace level for all stoat crates
//! STOAT_LOG=trace stoat gui
//! ```
//!
//! ## Environment Variable Priority
//!
//! 1. `STOAT_LOG` (highest priority) - Stoat-specific logging control
//! 2. `RUST_LOG` - Standard tracing environment variable
//! 3. Default - `warn` globally, `info` for stoat crates

pub mod text_proto;

use std::{env, fs};
pub use text_proto::{log_dir, TextProtoLog};
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize logging to `<XDG_STATE_HOME>/stoat/logs/stoat-<pid>.log`.
///
/// Writes to a file so tracing output never hits the raw-mode terminal.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = create_filter()?;
    let dir = log_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("stoat-{}.log", std::process::id()));
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init()?;
    Ok(())
}

fn create_filter() -> Result<EnvFilter, Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(stoat_log) = env::var("STOAT_LOG") {
        return Ok(expand_stoat_log(&stoat_log));
    }

    if let Ok(rust_log) = env::var("RUST_LOG") {
        return Ok(EnvFilter::new(rust_log));
    }

    Ok(EnvFilter::new("warn,stoat=info,stoat_bin=info"))
}

fn expand_stoat_log(stoat_log: &str) -> EnvFilter {
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::new(stoat_log);
    }

    EnvFilter::new(format!("warn,stoat={stoat_log},stoat_bin={stoat_log}"))
}
