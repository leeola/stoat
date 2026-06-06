#![allow(clippy::unwrap_used)]

use crate::{
    host::terminal::{SpawnArgs, TerminalHost, TerminalSession},
    run::{pty::PtyNotification, RunId},
};
use async_trait::async_trait;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub struct FakeTerminalSession {
    state: Mutex<FakeTerminalState>,
    read_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

struct FakeTerminalState {
    sent: Vec<Vec<u8>>,
    killed: bool,
    exit_code: Option<i32>,
    size: Option<(u16, u16)>,
    foreground_name: Option<String>,
}

impl FakeTerminalSession {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            state: Mutex::new(FakeTerminalState {
                sent: Vec::new(),
                killed: false,
                exit_code: None,
                size: None,
                foreground_name: None,
            }),
            read_tx: Mutex::new(Some(tx)),
            read_rx: tokio::sync::Mutex::new(rx),
        }
    }

    pub fn push_output(&self, data: &[u8]) {
        if let Some(tx) = self.read_tx.lock().unwrap().as_ref() {
            let _ = tx.try_send(data.to_vec());
        }
    }

    /// Set the name returned by [`TerminalSession::foreground_process_name`].
    pub fn set_foreground_name(&self, name: impl Into<String>) {
        self.state.lock().unwrap().foreground_name = Some(name.into());
    }

    /// Signal that the command finished with `exit_code`: closes the
    /// read channel so a pending `read` returns `Ok(0)`, and records the
    /// code for `try_wait`.
    pub fn finish(&self, exit_code: i32) {
        self.read_tx.lock().unwrap().take();
        self.state.lock().unwrap().exit_code = Some(exit_code);
    }

    pub fn sent_bytes(&self) -> Vec<Vec<u8>> {
        self.state.lock().unwrap().sent.clone()
    }

    pub fn sent_strings(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .sent
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    pub fn assert_sent(&self, index: usize, expected: &[u8]) {
        let state = self.state.lock().unwrap();
        assert_eq!(
            state.sent.get(index).map(Vec::as_slice),
            Some(expected),
            "sent[{index}] mismatch"
        );
    }

    pub fn assert_sent_count(&self, count: usize) {
        let state = self.state.lock().unwrap();
        assert_eq!(state.sent.len(), count, "send count mismatch");
    }

    pub fn was_killed(&self) -> bool {
        self.state.lock().unwrap().killed
    }

    /// The last `(rows, cols)` passed to [`TerminalSession::resize`], or
    /// `None` if it has not been resized.
    pub fn last_size(&self) -> Option<(u16, u16)> {
        self.state.lock().unwrap().size
    }
}

impl Default for FakeTerminalSession {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TerminalSession for FakeTerminalSession {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.state.lock().unwrap().sent.push(data.to_vec());
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut rx = self.read_rx.lock().await;
        match rx.recv().await {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                Ok(n)
            },
            None => Ok(0),
        }
    }

    async fn kill(&self) -> io::Result<()> {
        self.state.lock().unwrap().killed = true;
        Ok(())
    }

    async fn try_wait(&self) -> io::Result<Option<i32>> {
        Ok(self.state.lock().unwrap().exit_code)
    }

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.state.lock().unwrap().size = Some((rows, cols));
        Ok(())
    }

    fn foreground_process_name(&self) -> Option<String> {
        self.state.lock().unwrap().foreground_name.clone()
    }
}

/// Factory fake that hands out boxes wrapping the shared
/// `Arc<FakeTerminalSession>` so the harness's `terminal()` getter and
/// the box returned by [`Self::spawn`] see the same underlying state.
pub struct FakeTerminalHost {
    session: Arc<FakeTerminalSession>,
}

impl FakeTerminalHost {
    pub fn new(session: Arc<FakeTerminalSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl TerminalHost for FakeTerminalHost {
    async fn spawn(&self, _args: SpawnArgs) -> io::Result<Box<dyn TerminalSession>> {
        Ok(Box::new(ArcTerminalSession(self.session.clone())))
    }
}

/// Trait-object bridge so [`FakeTerminalHost::spawn`] can hand out a
/// `Box<dyn TerminalSession>` while the test harness retains its own
/// `Arc<FakeTerminalSession>` reference. Both paths target the same
/// underlying channels.
pub(crate) struct ArcTerminalSession(pub(crate) Arc<FakeTerminalSession>);

#[async_trait]
impl TerminalSession for ArcTerminalSession {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.0.write(data).await
    }

    async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).await
    }

    async fn kill(&self) -> io::Result<()> {
        self.0.kill().await
    }

    async fn try_wait(&self) -> io::Result<Option<i32>> {
        self.0.try_wait().await
    }

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.0.resize(rows, cols)
    }

    fn foreground_process_name(&self) -> Option<String> {
        self.0.foreground_process_name()
    }
}

pub fn inject_output(tx: &mpsc::Sender<PtyNotification>, run_id: RunId, data: &[u8]) {
    tx.try_send(PtyNotification::Output {
        run_id,
        data: data.to_vec(),
    })
    .expect("pty_tx send failed");
}

pub fn inject_done(tx: &mpsc::Sender<PtyNotification>, run_id: RunId, exit_code: i32) {
    tx.try_send(PtyNotification::CommandDone {
        run_id,
        exit_status: Some(exit_code),
    })
    .expect("pty_tx send failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn captures_writes() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            fake.write(b"hello").await.unwrap();
            fake.write(b"world").await.unwrap();
            assert_eq!(fake.sent_strings(), ["hello", "world"]);
            fake.assert_sent_count(2);
            fake.assert_sent(0, b"hello");
        });
    }

    #[test]
    fn tracks_kill() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            assert!(!fake.was_killed());
            fake.kill().await.unwrap();
            assert!(fake.was_killed());
        });
    }

    #[test]
    fn records_resize() {
        let fake = FakeTerminalSession::new();
        assert_eq!(fake.last_size(), None);
        fake.resize(40, 120).unwrap();
        assert_eq!(fake.last_size(), Some((40, 120)));
    }

    #[test]
    fn read_returns_pushed_data() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            fake.push_output(b"hello");
            let mut buf = [0u8; 64];
            let n = fake.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"hello");
        });
    }

    #[test]
    fn finish_records_exit_and_signals_eof() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            assert_eq!(fake.try_wait().await.unwrap(), None);

            fake.finish(7);
            assert_eq!(fake.try_wait().await.unwrap(), Some(7));

            let mut buf = [0u8; 8];
            assert_eq!(fake.read(&mut buf).await.unwrap(), 0);
        });
    }

    #[test]
    fn inject_delivers_notification() {
        let (tx, mut rx) = mpsc::channel(16);
        let run_id = RunId::default();
        inject_output(&tx, run_id, b"data");
        inject_done(&tx, run_id, 0);

        let notif = rx.try_recv().unwrap();
        assert!(matches!(notif, PtyNotification::Output { data, .. } if data == b"data"));
        let notif = rx.try_recv().unwrap();
        assert!(matches!(
            notif,
            PtyNotification::CommandDone {
                exit_status: Some(0),
                ..
            }
        ));
    }
}
