//! Byte-faithful append-only log for text-based wire protocols.
//!
//! Each call to [`TextProtoLog::record`] appends one payload followed by
//! `\n`, producing a pure JSONL file. The `&str` input type enforces UTF-8
//! and prevents accidental use with binary protocols.
//!
//! Direction (in/out) is encoded by keeping separate files, one
//! [`TextProtoLog`] instance per direction, so each file remains a
//! byte-faithful record of one half of a stream.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process,
    sync::Mutex,
};

/// Append-only log of text-protocol payloads.
pub struct TextProtoLog {
    writer: Mutex<Option<BufWriter<File>>>,
}

impl fmt::Debug for TextProtoLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextProtoLog").finish_non_exhaustive()
    }
}

impl TextProtoLog {
    /// Opens `<log_dir>/<kind>-<pid>.jsonl`, creating the directory if needed.
    pub fn create(kind: &str) -> io::Result<Self> {
        let dir = log_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{kind}-{}.jsonl", process::id()));
        Self::create_at(&path)
    }

    /// Opens an explicit path. The file is truncated if it exists.
    pub fn create_at(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            writer: Mutex::new(Some(BufWriter::new(file))),
        })
    }

    /// Appends `payload` followed by `\n`. Strips any trailing newline from
    /// `payload` first so output is strictly one-JSON-per-line.
    ///
    /// On I/O error, logs once via [`tracing::warn!`] and drops the writer so
    /// subsequent calls are no-ops. A failing transcript must never crash
    /// the host.
    pub fn record(&self, payload: &str) {
        let trimmed = payload.strip_suffix('\n').unwrap_or(payload);
        let mut guard = match self.writer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(writer) = guard.as_mut() else {
            return;
        };
        if let Err(e) = writer
            .write_all(trimmed.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
        {
            tracing::warn!("text_proto_log write failed, disabling: {e}");
            *guard = None;
        }
    }
}

/// Returns the base log directory: `<data_local_dir>/stoat/logs/`.
///
/// Does not create the directory. Callers that write files should ensure it
/// exists via [`std::fs::create_dir_all`].
pub fn log_dir() -> io::Result<PathBuf> {
    let base = dirs::data_local_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve data-local directory",
        )
    })?;
    Ok(base.join("stoat").join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    fn read_file(path: &Path) -> String {
        let mut buf = String::new();
        File::open(path)
            .expect("open log")
            .read_to_string(&mut buf)
            .expect("read log");
        buf
    }

    #[test]
    fn record_writes_payload_with_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        {
            let log = TextProtoLog::create_at(&path).unwrap();
            log.record(r#"{"a":1}"#);
            log.record(r#"{"b":2}"#);
        }
        assert_eq!(read_file(&path), "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn trailing_newline_stripped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        {
            let log = TextProtoLog::create_at(&path).unwrap();
            log.record("{\"a\":1}\n");
        }
        assert_eq!(read_file(&path), "{\"a\":1}\n");
    }

    #[test]
    fn create_truncates_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        std::fs::write(&path, "stale\n").unwrap();
        {
            let log = TextProtoLog::create_at(&path).unwrap();
            log.record(r#"{"fresh":true}"#);
        }
        assert_eq!(read_file(&path), "{\"fresh\":true}\n");
    }

    #[test]
    fn record_no_op_after_writer_dropped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        let log = TextProtoLog::create_at(&path).unwrap();
        log.record("first");
        *log.writer.lock().unwrap() = None;
        log.record("second");
        drop(log);
        assert_eq!(read_file(&path), "first\n");
    }

    #[test]
    fn log_dir_ends_in_stoat_logs() {
        let path = log_dir().expect("resolve log dir");
        let mut components: Vec<_> = path
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_owned())
            .collect();
        components.reverse();
        assert_eq!(
            components,
            [
                std::ffi::OsString::from("stoat"),
                std::ffi::OsString::from("logs"),
            ]
        );
    }
}
