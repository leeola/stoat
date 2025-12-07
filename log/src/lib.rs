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

use std::env;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize logging.
///
/// Respects environment variable priority: `STOAT_LOG` > `RUST_LOG` > defaults.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = create_filter()?;
    fmt().with_env_filter(filter).try_init()?;
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
