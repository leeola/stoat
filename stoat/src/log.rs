//! Logging setup for Stoat with user-friendly environment variable controls.
//!
//! ## User Experience
//!
//! ### Basic Usage (90% of users)
//!
//! ```bash
//! # Default: quiet, only important messages
//! stoat
//!
//! # Debug level for all stoat crates
//! STOAT_LOG=debug stoat
//!
//! # Trace level for all stoat crates
//! STOAT_LOG=trace stoat
//! ```
//!
//! ### Advanced Usage (power users)
//!
//! ```bash
//! # Module-specific levels using STOAT_LOG
//! STOAT_LOG=stoat_core=debug,stoat_gui=trace stoat
//!
//! # Complex filtering with any tracing syntax
//! STOAT_LOG=warn,stoat_core::node=trace,iced=error stoat
//!
//! # Full control with RUST_LOG (STOAT_LOG takes precedence if both set)
//! RUST_LOG=debug stoat
//! ```
//!
//! ## Environment Variable Priority
//!
//! 1. **`STOAT_LOG`** (highest priority) - Stoat-specific logging control
//! 2. **`RUST_LOG`** - Standard tracing environment variable
//! 3. **Default** - `warn` globally, `info` for stoat crates
//!
//! ## Implementation Details
//!
//! The [`init`] function uses [`tracing_subscriber`] with [`EnvFilter`] to provide
//! logging control. All functions are designed to be safe - they will not crash if called
//! multiple times or if logging is already initialized.

use std::env;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize logging for production use.
///
/// This function respects the environment variable priority described in the module docs:
/// [`STOAT_LOG`] > [`RUST_LOG`] > default settings.
///
/// Safe to call multiple times - will not crash if logging is already initialized.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = create_filter()?;
    fmt().with_env_filter(filter).try_init()?;
    Ok(())
}

/// Initialize logging for tests.
///
/// Identical to [`init`] but with a name that makes it clear this is safe for test usage.
/// Will not crash if called multiple times or if logging is already initialized by another test.
#[allow(clippy::let_unit_value)]
pub fn test() {
    let _ = init();
}

/// Create the appropriate [`EnvFilter`] based on environment variables.
///
/// Implements the priority system: [`STOAT_LOG`] > [`RUST_LOG`] > defaults.
fn create_filter() -> Result<EnvFilter, Box<dyn std::error::Error + Send + Sync>> {
    // Priority order:
    // 1. STOAT_LOG - if set, expand it to stoat namespaces (highest priority)
    // 2. RUST_LOG (standard tracing env var) - if set, use it directly
    // 3. Default - warn globally, info for stoat crates

    if let Ok(stoat_log) = env::var("STOAT_LOG") {
        return Ok(expand_stoat_log(&stoat_log));
    }

    if let Ok(rust_log) = env::var("RUST_LOG") {
        return Ok(EnvFilter::new(rust_log));
    }

    // Default: warn globally, info for stoat crates
    Ok(EnvFilter::new(
        "warn,stoat=info,stoat_core=info,stoat_bin=info,stoat_gui=info,stoat_text=info",
    ))
}

/// Expand [`STOAT_LOG`] values into full tracing filter strings.
///
/// This function provides the user-friendly experience where:
/// - `STOAT_LOG=debug` becomes `warn,stoat=debug,stoat_core=debug,...`
/// - `STOAT_LOG=stoat_core=trace,stoat=debug` is used as-is (advanced syntax)
///
/// # Arguments
///
/// * `stoat_log` - The value of the [`STOAT_LOG`] environment variable
///
/// # Returns
///
/// An [`EnvFilter`] configured according to the [`STOAT_LOG`] value.
fn expand_stoat_log(stoat_log: &str) -> EnvFilter {
    // If the STOAT_LOG contains module-specific syntax (contains '=', ':', or ','),
    // use it as-is to allow advanced usage like STOAT_LOG=stoat_core=debug,stoat_gui=trace
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::new(stoat_log);
    }

    // Otherwise, treat it as a simple level and apply to all stoat crates
    EnvFilter::new(format!(
        "warn,stoat={stoat_log},stoat_core={stoat_log},stoat_bin={stoat_log},stoat_gui={stoat_log},stoat_text={stoat_log}"
    ))
}
