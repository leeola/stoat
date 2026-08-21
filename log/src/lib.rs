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
/// `stoat_log` takes precedence over `rust_log`; both `None` falls back to
/// `warn` plus every [`DEFAULT_TARGETS`] crate at info. Callers resolve env
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

    Ok(EnvFilter::new(default_directives("info")))
}

fn expand_stoat_log(stoat_log: &str) -> Result<EnvFilter, LogInitError> {
    if stoat_log.contains('=') || stoat_log.contains(':') || stoat_log.contains(',') {
        return EnvFilter::try_new(stoat_log).context(BuildEnvFilterSnafu);
    }

    EnvFilter::try_new(default_directives(stoat_log)).context(BuildEnvFilterSnafu)
}

#[cfg(test)]
mod tests {
    use super::{create_filter, default_directives};
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
    fn the_default_names_every_target_at_info() {
        assert_eq!(directives(None, None), expected("info"));
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
}
