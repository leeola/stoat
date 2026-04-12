//! Byte-faithful append-only log for text-based wire protocols.
//!
//! Each call to [`TextProtoLog::record`] enqueues one payload followed by
//! `\n`, producing a pure JSONL file.
//!
//! Direction (in/out) is encoded by keeping separate files, one
//! [`TextProtoLog`] instance per direction, so each file remains a
//! byte-faithful record of one half of a stream.
//!
//! # Concurrency model
//!
//! Writes are performed on a dedicated `std::thread` that owns the
//! underlying [`BufWriter<File>`]. Callers enqueue payloads through a
//! bounded channel and return immediately without blocking on disk I/O.
//! This keeps the tokio stdin/stdout handler tasks off any blocking
//! syscall path.
//!
//! If the disk stalls and the channel fills, [`TextProtoLog::record`]
//! drops the incoming payload and increments a counter. On the next
//! successful record the writer thread prepends a
//! `{"dropped": N}` breadcrumb so the transcript remains self-describing.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError},
        Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Bound on the writer channel. Chosen to give several seconds of
/// headroom under a stalled disk before records start dropping, without
/// letting the in-memory queue grow without bound.
const CHANNEL_CAPACITY: usize = 4096;

/// Time [`TextProtoLog::flush`] will wait for the writer thread to
/// acknowledge the flush before giving up. Chosen so shutdown never
/// stalls for more than half a second on a pathological writer.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

enum WriterMessage {
    Record(String),
    Flush(SyncSender<()>),
}

/// Append-only log of text-protocol payloads.
pub struct TextProtoLog {
    /// Sender half of the writer channel. `None` once the writer thread
    /// has exited (e.g. because an I/O error disabled it), after which
    /// [`TextProtoLog::record`] becomes a silent no-op.
    sender: Mutex<Option<SyncSender<WriterMessage>>>,

    /// Count of records dropped because the channel was full. The writer
    /// thread reads and resets this on every successful write so it can
    /// emit a `{"dropped": N}` breadcrumb line.
    dropped: std::sync::Arc<AtomicU64>,

    /// Handle to the dedicated writer thread. Joined on drop.
    thread: Mutex<Option<JoinHandle<()>>>,
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

        let (sender, receiver) = sync_channel::<WriterMessage>(CHANNEL_CAPACITY);
        let dropped = std::sync::Arc::new(AtomicU64::new(0));
        let writer = BufWriter::new(file);
        let thread_dropped = dropped.clone();

        let thread = thread::Builder::new()
            .name(format!(
                "text-proto-log-{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ))
            .spawn(move || writer_loop(writer, receiver, thread_dropped))
            .map_err(|e| io::Error::other(format!("spawn writer thread: {e}")))?;

        Ok(Self {
            sender: Mutex::new(Some(sender)),
            dropped,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Enqueues `payload` for the writer thread. Strips any trailing
    /// newline so output is strictly one-JSON-per-line.
    ///
    /// Never blocks on disk I/O. If the writer's backlog is full the
    /// payload is dropped and a counter is incremented; the next
    /// successful record will surface the drop count as a breadcrumb in
    /// the log.
    pub fn record(&self, payload: &str) {
        let trimmed = payload.strip_suffix('\n').unwrap_or(payload);
        let mut guard = match self.sender.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(sender) = guard.as_ref() else {
            return;
        };
        match sender.try_send(WriterMessage::Record(trimmed.to_owned())) {
            Ok(()) => {},
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            },
            Err(TrySendError::Disconnected(_)) => {
                *guard = None;
            },
        }
    }

    /// Blocks until the writer thread has flushed every prior
    /// [`TextProtoLog::record`] call to disk, or [`FLUSH_TIMEOUT`]
    /// elapses. Silent on timeout so callers never stall shutdown.
    pub fn flush(&self) {
        let sender = {
            let guard = match self.sender.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.as_ref() {
                Some(s) => s.clone(),
                None => return,
            }
        };
        let (ack_tx, ack_rx) = sync_channel::<()>(1);
        if sender.send(WriterMessage::Flush(ack_tx)).is_err() {
            return;
        }
        match ack_rx.recv_timeout(FLUSH_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {},
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("text_proto_log flush timed out after {FLUSH_TIMEOUT:?}");
            },
        }
    }
}

impl Drop for TextProtoLog {
    fn drop(&mut self) {
        self.flush();
        // Drop the sender so the writer thread's `recv()` returns Err
        // and its loop exits naturally. The `thread.join()` that follows
        // then completes once the thread finishes its final flush.
        if let Ok(mut guard) = self.sender.lock() {
            guard.take();
        }
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn writer_loop(
    mut writer: BufWriter<File>,
    receiver: std::sync::mpsc::Receiver<WriterMessage>,
    dropped: std::sync::Arc<AtomicU64>,
) {
    let mut disabled = false;

    while let Ok(msg) = receiver.recv() {
        match msg {
            WriterMessage::Record(payload) => {
                if disabled {
                    continue;
                }
                if let Err(e) = write_record(&mut writer, &dropped, &payload) {
                    tracing::warn!("text_proto_log write failed, disabling: {e}");
                    disabled = true;
                }
            },
            WriterMessage::Flush(ack) => {
                if !disabled {
                    if let Err(e) = writer.flush() {
                        tracing::warn!("text_proto_log flush failed: {e}");
                    }
                }
                let _ = ack.send(());
            },
        }
    }

    if !disabled {
        let _ = writer.flush();
    }
}

fn write_record(
    writer: &mut BufWriter<File>,
    dropped: &AtomicU64,
    payload: &str,
) -> io::Result<()> {
    let drop_count = dropped.swap(0, Ordering::Relaxed);
    if drop_count > 0 {
        let breadcrumb = format!("{{\"dropped\":{drop_count}}}");
        writer.write_all(breadcrumb.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
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
    fn flush_persists_records_without_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        let log = TextProtoLog::create_at(&path).unwrap();
        log.record(r#"{"a":1}"#);
        log.flush();
        assert_eq!(read_file(&path), "{\"a\":1}\n");
        drop(log);
    }

    #[test]
    fn stress_10k_records_no_drops() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.jsonl");
        {
            let log = TextProtoLog::create_at(&path).unwrap();
            for i in 0..10_000 {
                log.record(&format!("{{\"n\":{i}}}"));
            }
            log.flush();
            assert_eq!(
                log.dropped.load(Ordering::Relaxed),
                0,
                "no drops expected on a non-stalled disk"
            );
        }
        let contents = read_file(&path);
        let line_count = contents.lines().count();
        assert_eq!(line_count, 10_000);
        assert!(contents.starts_with("{\"n\":0}\n"));
        assert!(contents.ends_with("{\"n\":9999}\n"));
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
