//! Logging setup for Stoat with file output and optional stdout.
//!
//! Logs always go to a file at `warn` level (or higher if `--log` is set).
//! Stdout logging is enabled when `STOAT_LOG` or `RUST_LOG` is set, or in debug builds.
//!
//! ## Environment Variables
//!
//! 1. **`STOAT_LOG`** (highest priority) - Stoat-specific logging control
//! 2. **`RUST_LOG`** - Standard tracing environment variable
//! 3. **Default** - `warn` globally, `info` for stoat crates
//!
//! ## Log File Location
//!
//! Default: `<data_local_dir>/stoat/logs/stoat-<pid>.log`
//! - macOS: `~/Library/Application Support/stoat/logs/stoat-12345.log`
//! - Linux: `~/.local/share/stoat/logs/stoat-12345.log`
//!
//! Override with `--log-file <path>` or `STOAT_LOG_FILE`.

use std::{env, path::PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

/// Returned from [`init`]; must be held alive to ensure log file flushing.
pub struct LogGuard {
    _file_guard: WorkerGuard,
    pub log_file: PathBuf,
}

pub struct LogConfig {
    pub log_file_path: Option<PathBuf>,
}

/// Initialize logging.
///
/// This function respects the environment variable priority described in the module docs:
/// [`STOAT_LOG`] > [`RUST_LOG`] > default settings.
///
/// The returned [`LogGuard`] must be held for the lifetime of the program --
/// dropping it flushes and stops the background file writer.
///
/// Safe to call multiple times -- will not crash if logging is already initialized.
pub fn init(config: LogConfig) -> Result<LogGuard, Box<dyn std::error::Error + Send + Sync>> {
    let log_dir_and_filename = resolve_log_path(config.log_file_path);
    let (log_dir, filename) = match &log_dir_and_filename {
        Some((dir, name)) => (dir.as_path(), name.as_str()),
        None => unreachable!(),
    };

    std::fs::create_dir_all(log_dir).ok();

    let file_appender = tracing_appender::rolling::never(log_dir, filename);
    let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = create_file_filter()?;
    let file_layer = fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_filter(file_filter);

    let stdout_enabled =
        env::var("STOAT_LOG").is_ok() || env::var("RUST_LOG").is_ok() || cfg!(debug_assertions);

    let stdout_layer = if stdout_enabled {
        Some(fmt::layer().with_filter(create_filter()?))
    } else {
        None
    };

    Registry::default()
        .with(file_layer)
        .with(stdout_layer)
        .try_init()?;

    Ok(LogGuard {
        _file_guard: file_guard,
        log_file: log_dir.join(filename),
    })
}

/// Initialize logging for tests.
///
/// Identical to [`init`] but stdout-only (no file output), with a name that makes it
/// clear this is safe for test usage. Will not crash if called multiple times or if
/// logging is already initialized by another test.
#[allow(clippy::let_unit_value)]
pub fn test() {
    let _ = test_init();
}

fn test_init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = create_filter()?;
    fmt().with_env_filter(filter).try_init()?;
    Ok(())
}

fn resolve_log_path(override_path: Option<PathBuf>) -> Option<(PathBuf, String)> {
    let filename = format!("stoat-{}.log", std::process::id());

    if let Some(path) = override_path {
        if path.extension().is_some() {
            let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(filename);
            return Some((dir.to_path_buf(), name));
        }
        return Some((path, filename));
    }

    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("stoat")
        .join("logs");

    Some((dir, filename))
}

/// File filter: uses user-specified level if set, otherwise defaults to `warn`.
fn create_file_filter() -> Result<EnvFilter, Box<dyn std::error::Error + Send + Sync>> {
    if env::var("STOAT_LOG").is_ok() || env::var("RUST_LOG").is_ok() {
        return create_filter();
    }
    Ok(EnvFilter::new("warn"))
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
        "warn,stoat=info,stoat_core=info,stoat_bin=info,stoat_gui=info,stoat_text=info,stoat_display_map=info",
    ))
}

/// Expand [`STOAT_LOG`] values into full tracing filter strings.
///
/// This function provides the user-friendly experience where:
/// - `STOAT_LOG=debug` becomes `warn,stoat=debug,stoat_core=debug,...`
/// - `STOAT_LOG=stoat_core=trace,stoat=debug` is used as-is (advanced syntax)
fn expand_stoat_log(stoat_log: &str) -> EnvFilter {
    // If the STOAT_LOG contains module-specific syntax (contains '=', ':', or ','),
    // use it as-is to allow advanced usage like STOAT_LOG=stoat_core=debug,stoat_gui=trace
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::new(stoat_log);
    }

    EnvFilter::new(format!(
        "warn,stoat={stoat_log},stoat_core={stoat_log},stoat_bin={stoat_log},stoat_gui={stoat_log},stoat_text={stoat_log},stoat_display_map={stoat_log}"
    ))
}
