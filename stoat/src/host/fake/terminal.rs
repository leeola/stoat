use crate::{
    host::terminal::TerminalHost,
    run::{pty::PtyNotification, RunId},
};
use async_trait::async_trait;
use std::{io, sync::Mutex};
use tokio::sync::mpsc;

pub struct FakeTerminal {
    state: Mutex<FakeTerminalState>,
    read_tx: mpsc::Sender<Vec<u8>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

struct FakeTerminalState {
    sent: Vec<Vec<u8>>,
    killed: bool,
}

impl FakeTerminal {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            state: Mutex::new(FakeTerminalState {
                sent: Vec::new(),
                killed: false,
            }),
            read_tx: tx,
            read_rx: tokio::sync::Mutex::new(rx),
        }
    }

    pub fn push_output(&self, data: &[u8]) {
        let _ = self.read_tx.try_send(data.to_vec());
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
}

#[async_trait]
impl TerminalHost for FakeTerminal {
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
            let fake = FakeTerminal::new();
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
            let fake = FakeTerminal::new();
            assert!(!fake.was_killed());
            fake.kill().await.unwrap();
            assert!(fake.was_killed());
        });
    }

    #[test]
    fn read_returns_pushed_data() {
        rt().block_on(async {
            let fake = FakeTerminal::new();
            fake.push_output(b"hello");
            let mut buf = [0u8; 64];
            let n = fake.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"hello");
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
