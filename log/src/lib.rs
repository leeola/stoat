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

pub mod ident;
pub mod paths;
pub mod text_proto;

pub use paths::{data_dir, state_dir, workspace_state_dir};
use snafu::{ResultExt, Snafu};
use std::{fs, io, path::PathBuf};
pub use text_proto::{log_dir, TextProtoLog};
use tracing_subscriber::{
    filter::ParseError,
    fmt,
    util::{SubscriberInitExt, TryInitError},
    EnvFilter,
};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum LogInitError {
    #[snafu(display("Failed to open log file: {}", path.display()))]
    OpenLogFile {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to parse log filter directive"))]
    BuildEnvFilter {
        source: ParseError,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to install global tracing subscriber"))]
    SetGlobalSubscriber {
        source: TryInitError,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Where [`init`] routes tracing output.
pub enum LogTarget {
    /// Append to the file at this path with ANSI disabled, so the raw bytes
    /// stay clean and tracing never corrupts the raw-mode terminal.
    File(PathBuf),
    /// Write to stderr with ANSI enabled for live console viewing. The raw-mode
    /// TUI is corrupted by stderr unless the caller redirects it (`2>file`).
    Stderr,
}

/// Initialize logging to `target`.
///
/// `LogTarget::File` appends to the file (creating it if needed) with ANSI off,
/// so tracing output never hits the raw-mode terminal. `LogTarget::Stderr`
/// writes to stderr with ANSI on for live viewing, which the raw-mode TUI needs
/// redirected (`2>file`) to stay readable.
///
/// `stoat_log` takes precedence over `rust_log`; both `None` falls back to the
/// compiled-in default of `warn,stoat=info,stoat_bin=info`. Callers resolve env
/// state and the log file path at the binary boundary, including ensuring the
/// parent directory exists; this crate does not read the process environment or
/// create directories.
pub fn init(
    stoat_log: Option<String>,
    rust_log: Option<String>,
    target: LogTarget,
) -> Result<(), LogInitError> {
    let filter = create_filter(stoat_log, rust_log)?;
    match target {
        LogTarget::File(path) => {
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|_| OpenLogFileSnafu { path: path.clone() })?;
            fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .finish()
                .try_init()
                .context(SetGlobalSubscriberSnafu)?;
        },
        LogTarget::Stderr => {
            fmt()
                .with_env_filter(filter)
                .with_writer(io::stderr)
                .with_ansi(true)
                .finish()
                .try_init()
                .context(SetGlobalSubscriberSnafu)?;
        },
    }
    Ok(())
}

fn create_filter(
    stoat_log: Option<String>,
    rust_log: Option<String>,
) -> Result<EnvFilter, LogInitError> {
    if let Some(stoat_log) = stoat_log {
        return expand_stoat_log(&stoat_log);
    }

    if let Some(rust_log) = rust_log {
        return EnvFilter::try_new(rust_log).context(BuildEnvFilterSnafu);
    }

    Ok(EnvFilter::new("warn,stoat=info,stoat_bin=info"))
}

fn expand_stoat_log(stoat_log: &str) -> Result<EnvFilter, LogInitError> {
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::try_new(stoat_log).context(BuildEnvFilterSnafu);
    }

    EnvFilter::try_new(format!("warn,stoat={stoat_log},stoat_bin={stoat_log}"))
        .context(BuildEnvFilterSnafu)
}
