use std::{fmt, sync::OnceLock};
use time::{macros::format_description, OffsetDateTime};

/// A timestamped, collision-free identifier for one process's run.
///
/// Rendered as `yyyymmdd-hhmmss-<pid>` in UTC. The timestamp orders a session's
/// log files chronologically and the pid disambiguates two processes that mint
/// an id in the same second, so the id is unique without pulling in a random
/// source. It is the key that correlates the files (editor log, LSP transcripts)
/// belonging to a single run, and that a paired stoatty/stoat exchange logs to
/// tie the two sides together.
pub struct LogId(String);

impl LogId {
    /// Builds an id from an explicit UTC `timestamp` and `pid`. Kept separate
    /// from [`Self::mint`] so the formatting is testable without reading the
    /// clock.
    pub fn new(timestamp: OffsetDateTime, pid: u32) -> Self {
        let format = format_description!("[year][month][day]-[hour][minute][second]");
        let stamp = timestamp
            .format(&format)
            .expect("a complete UTC timestamp always formats");
        LogId(format!("{stamp}-{pid}"))
    }

    /// Mints an id from the current UTC time and this process's pid.
    pub fn mint() -> Self {
        Self::new(OffsetDateTime::now_utc(), std::process::id())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A process's installed identity: its [`LogId`] and the stem its log files are
/// named with.
///
/// `file_stem` is the log filename minus the `.log` extension. It is retained
/// rather than recomputed so downstream files that must sort next to the main
/// log -- LSP transcripts especially -- can reuse the exact stem instead of
/// re-deriving it from the id and prefix.
pub struct ProcessIdent {
    pub id: LogId,
    pub file_stem: String,
}

static PROCESS_IDENT: OnceLock<ProcessIdent> = OnceLock::new();

/// Records this process's identity for later retrieval via [`get`].
///
/// The first write wins and a second call is a no-op, so the binary can install
/// once at startup and every later reader sees the same identity.
pub fn install(ident: ProcessIdent) {
    let _ = PROCESS_IDENT.set(ident);
}

/// Returns the identity set by [`install`], or `None` when nothing has installed
/// one yet (library callers and tests that never named a log file).
pub fn get() -> Option<&'static ProcessIdent> {
    PROCESS_IDENT.get()
}

/// Returns this machine's hostname, or `"unknown"` when it cannot be read.
///
/// Used to label a log so a session driven over ssh is attributable to the
/// machine it ran on. Best-effort by design: a lookup failure or a non-unix
/// target yields `"unknown"` rather than propagating an error into logging
/// setup.
#[cfg(unix)]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return "unknown".to_string();
    }

    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
pub fn hostname() -> String {
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn log_id_renders_timestamp_and_pid() {
        let id = LogId::new(datetime!(2026-07-18 14:30:22 UTC), 12345);
        assert_eq!(id.as_str(), "20260718-143022-12345");
    }
}
