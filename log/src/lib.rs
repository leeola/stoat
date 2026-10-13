//! Logging setup for Stoat.
//!
//! ## Usage
//!
//! ```bash
//! # Default: debug for the stoat crates, warn for everything else
//! stoat gui
//!
//! # Info level for all stoat crates
//! STOAT_LOG=info stoat gui
//!
//! # Trace level for all stoat crates
//! STOAT_LOG=trace stoat gui
//! ```
//!
//! ## Environment Variable Priority
//!
//! 1. `STOAT_LOG` (highest priority) - Stoat-specific logging control
//! 2. `RUST_LOG` - Standard tracing environment variable
//! 3. Default - `warn` globally, `debug` for stoat crates
//!
//! [`set_level`] replaces the default after init, as a config does. Either
//! variable pins its own filter, so `set_level` leaves that filter in place.

pub mod ident;
pub mod paths;
#[cfg(unix)]
mod sockets;
pub mod text_proto;

pub use paths::{data_dir, state_dir, workspace_state_dir};
use snafu::{ResultExt, Snafu};
#[cfg(unix)]
pub use sockets::{socket_bind_dir, socket_dir};
use std::{fs, io, path::PathBuf, sync::OnceLock};
pub use text_proto::{log_dir, TextProtoLog};
use tracing_subscriber::{
    filter::ParseError,
    fmt,
    layer::SubscriberExt,
    reload,
    util::{SubscriberInitExt, TryInitError},
    EnvFilter, Registry,
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

    #[snafu(display("Failed to replace the log filter"))]
    ReloadFilter {
        source: reload::Error,
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

/// The reload handle of the filter [`init`] installed, and whether
/// `STOAT_LOG` or `RUST_LOG` chose that filter.
struct FilterHandle {
    handle: reload::Handle<EnvFilter, Registry>,
    pinned_by_env: bool,
}

/// The filter [`init`] installed, which [`set_level`] replaces.
static FILTER: OnceLock<FilterHandle> = OnceLock::new();

/// Initialize logging to `target`.
///
/// `LogTarget::File` appends to the file (creating it if needed) with ANSI off,
/// so tracing output never hits the raw-mode terminal. `LogTarget::Stderr`
/// writes to stderr with ANSI on for live viewing, which the raw-mode TUI needs
/// redirected (`2>file`) to stay readable.
///
/// `stoat_log` takes precedence over `rust_log`. With both `None`, the filter
/// is `warn` plus every [`DEFAULT_TARGETS`] crate at debug, and [`set_level`]
/// replaces it once a config loads.
///
/// Callers resolve env state and the log file path at the binary boundary,
/// including ensuring the parent directory exists. This crate does not read the
/// process environment or create directories.
pub fn init(
    stoat_log: Option<String>,
    rust_log: Option<String>,
    target: LogTarget,
) -> Result<(), LogInitError> {
    let pinned_by_env = stoat_log.is_some() || rust_log.is_some();
    let (filter, handle) = reload::Layer::new(create_filter(stoat_log, rust_log)?);
    let registry = tracing_subscriber::registry().with(filter);
    match target {
        LogTarget::File(path) => {
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|_| OpenLogFileSnafu { path: path.clone() })?;
            registry
                .with(fmt::layer().with_writer(file).with_ansi(false))
                .try_init()
                .context(SetGlobalSubscriberSnafu)?;
        },
        LogTarget::Stderr => {
            registry
                .with(fmt::layer().with_writer(io::stderr).with_ansi(true))
                .try_init()
                .context(SetGlobalSubscriberSnafu)?;
        },
    }
    let _ = FILTER.set(FilterHandle {
        handle,
        pinned_by_env,
    });
    Ok(())
}

/// Replace the filter [`init`] installed with `level`, as a config's log level
/// asks.
///
/// `level` is a bare level such as `debug`, which applies to every
/// [`DEFAULT_TARGETS`] crate over a `warn` floor, or a directive string as
/// `STOAT_LOG` takes it. A level that does not parse is an error.
///
/// A process with no installed subscriber accepts any level that parses and
/// changes nothing. An [`init`] that read `STOAT_LOG` or `RUST_LOG` keeps the
/// filter of the environment, because a variable is an explicit request for
/// one launch, and logs one info line that says so.
pub fn set_level(level: &str) -> Result<(), LogInitError> {
    let filter = expand_stoat_log(level)?;
    let Some(installed) = FILTER.get() else {
        return Ok(());
    };
    if installed.pinned_by_env {
        tracing::info!(
            target: "stoat_log",
            level,
            "config log level ignored, because STOAT_LOG or RUST_LOG is set"
        );
        return Ok(());
    }
    installed.handle.reload(filter).context(ReloadFilterSnafu)
}

/// The crate targets the compiled-in default and the bare-level `STOAT_LOG`
/// form raise above `warn`.
///
/// A directive matches a target by raw string prefix, so `stoat` alone already
/// reaches every stoatty target. Naming them is what makes that reach
/// deliberate rather than an accident of two crate names sharing five letters:
/// the terminal's own startup telemetry is what the perf work reads, and it
/// should not depend on a prefix compare staying a prefix compare.
const DEFAULT_TARGETS: [&str; 6] = [
    "stoat",
    "stoat_bin",
    "stoatty",
    "stoatty_term",
    "stoatty_render",
    "stoatty_protocol",
];

/// `warn`, plus every [`DEFAULT_TARGETS`] entry at `level`.
///
/// One builder for the compiled-in default and the bare-level `STOAT_LOG`
/// form, so the two cannot name different sets.
fn default_directives(level: &str) -> String {
    let mut out = String::from("warn");
    for target in DEFAULT_TARGETS {
        out.push(',');
        out.push_str(target);
        out.push('=');
        out.push_str(level);
    }
    out
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

    Ok(EnvFilter::new(default_directives("debug")))
}

fn expand_stoat_log(stoat_log: &str) -> Result<EnvFilter, LogInitError> {
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::try_new(stoat_log).context(BuildEnvFilterSnafu);
    }

    EnvFilter::try_new(default_directives(stoat_log)).context(BuildEnvFilterSnafu)
}

#[cfg(test)]
mod tests {
    use super::{create_filter, default_directives, set_level, LogInitError};
    use tracing::Level;
    use tracing_subscriber::fmt;

    /// The crates the default set names, spelled out here rather than read
    /// from [`super::DEFAULT_TARGETS`].
    ///
    /// Deriving the expectation from the value under test would accept any
    /// name the constant happens to hold, including a typo that reaches
    /// nothing. A behavioral probe cannot close that gap either: `stoat`
    /// prefix-matches every stoatty target, so a misspelled entry still lets
    /// the event through and only the directive set tells the two apart.
    const NAMED: [&str; 6] = [
        "stoat",
        "stoat_bin",
        "stoatty",
        "stoatty_term",
        "stoatty_render",
        "stoatty_protocol",
    ];

    /// The parsed directives of the filter `stoat_log` and `rust_log` resolve
    /// to, as the set of `target=level` strings it holds.
    ///
    /// Reading the filter back rather than the string it was built from is what
    /// separates "the name was written" from "the name became a directive".
    fn directives(stoat_log: Option<&str>, rust_log: Option<&str>) -> Vec<String> {
        let filter = create_filter(stoat_log.map(str::to_owned), rust_log.map(str::to_owned))
            .expect("the filter builds");
        let mut parsed: Vec<String> = filter.to_string().split(',').map(str::to_owned).collect();
        parsed.sort();
        parsed
    }

    /// Every [`NAMED`] crate at `level`, plus the bare `warn` floor, sorted for
    /// comparing against a filter's own directives.
    fn expected(level: &str) -> Vec<String> {
        let mut want: Vec<String> = NAMED
            .iter()
            .map(|target| format!("{target}={level}"))
            .collect();
        want.push("warn".to_owned());
        want.sort();
        want
    }

    /// The cold-start telemetry the perf work reads comes from the stoatty
    /// crates, and the default has to name them rather than reach them through
    /// a prefix that happens to match.
    #[test]
    fn the_default_names_every_target_at_debug() {
        assert_eq!(directives(None, None), expected("debug"));
    }

    /// The bare-level form builds its own string, so it has to name the same
    /// set. Two hand-written lists drift; one builder cannot.
    #[test]
    fn a_bare_stoat_log_level_names_the_same_targets() {
        assert_eq!(directives(Some("debug"), None), expected("debug"));
    }

    /// A `STOAT_LOG` carrying its own directives is passed through, so a user
    /// who writes one is not overruled by the default set.
    #[test]
    fn a_directive_stoat_log_passes_through_untouched() {
        assert_eq!(
            directives(Some("stoat_language=trace"), None),
            vec!["stoat_language=trace".to_owned()]
        );
    }

    /// `STOAT_LOG` wins over `RUST_LOG`, and `RUST_LOG` alone is passed
    /// through, so neither one silently picks up the default set.
    #[test]
    fn rust_log_is_used_alone_and_yields_to_stoat_log() {
        assert_eq!(
            directives(None, Some("wgpu_core=warn")),
            vec!["wgpu_core=warn".to_owned()]
        );
        assert_eq!(
            directives(Some("stoat_text=trace"), Some("wgpu_core=warn")),
            vec!["stoat_text=trace".to_owned()]
        );
    }

    /// The directives are what a real event is measured against, so one event
    /// goes through a subscriber to show the set reaches the level it names.
    /// The subscriber is scoped to this call and leaves nothing behind.
    #[test]
    fn an_info_event_on_a_stoatty_target_passes_the_default() {
        let filter = create_filter(None, None).expect("the filter builds");
        let subscriber = fmt().with_env_filter(filter).finish();

        let (admitted, refused) = tracing::subscriber::with_default(subscriber, || {
            (
                tracing::event_enabled!(target: "stoatty_render::gpu", Level::INFO),
                tracing::event_enabled!(target: "wgpu_core::device", Level::INFO),
            )
        });
        assert_eq!(
            (admitted, refused),
            (true, false),
            "the named crates rise to info and everything else stays at warn"
        );
    }

    /// Default directives never come out empty or unsorted into one blob, so a
    /// caller reading the filter back sees one entry per named crate.
    #[test]
    fn default_directives_names_each_target_once() {
        let built = default_directives("info");
        assert_eq!(
            built.matches("=info").count(),
            NAMED.len(),
            "one directive per named crate: {built}"
        );
        assert!(built.starts_with("warn,"), "the floor comes first: {built}");
    }

    #[test]
    fn set_level_rejects_an_unknown_level() {
        for level in ["bogus", "stoat=bogus"] {
            assert!(
                matches!(set_level(level), Err(LogInitError::BuildEnvFilter { .. })),
                "{level} does not parse"
            );
        }
    }

    /// No test installs a global subscriber, so this process holds no filter
    /// to replace.
    #[test]
    fn set_level_without_a_subscriber_accepts_a_level() {
        assert!(matches!(set_level("trace"), Ok(())));
    }
}
