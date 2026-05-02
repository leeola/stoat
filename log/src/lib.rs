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

pub mod paths;
pub mod text_proto;

pub use paths::{data_dir, state_dir, workspace_state_dir};
use snafu::{ResultExt, Snafu};
use std::{fs, io, path::PathBuf};
pub use text_proto::{log_dir, TextProtoLog};
use tracing_subscriber::{filter::ParseError, fmt, EnvFilter};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum LogInitError {
    #[snafu(display("Failed to resolve log directory"))]
    ResolveLogDir {
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to create log directory: {}", path.display()))]
    CreateLogDir {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

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
        source: Box<dyn std::error::Error + Send + Sync>,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Initialize logging to `<XDG_STATE_HOME>/stoat/logs/stoat-<pid>.log`.
///
/// Writes to a file so tracing output never hits the raw-mode terminal.
/// `stoat_log` takes precedence over `rust_log`; both `None` falls back
/// to the compiled-in default of `warn,stoat=info,stoat_bin=info`.
/// Callers resolve env state at the binary boundary; this crate does
/// not read the process environment.
pub fn init(stoat_log: Option<String>, rust_log: Option<String>) -> Result<(), LogInitError> {
    let filter = create_filter(stoat_log, rust_log)?;
    let dir = log_dir().context(ResolveLogDirSnafu)?;
    fs::create_dir_all(&dir).with_context(|_| CreateLogDirSnafu { path: dir.clone() })?;
    let path = dir.join(format!("stoat-{}.log", std::process::id()));
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|_| OpenLogFileSnafu { path: path.clone() })?;
    fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init()
        .context(SetGlobalSubscriberSnafu)?;
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
