use async_trait::async_trait;
use std::{io, sync::Mutex};
use tokio::sync::mpsc;

#[async_trait]
pub trait TerminalHost: Send + Sync {
    async fn write(&self, data: &[u8]) -> io::Result<()>;
    /// The next chunk of shell output, or `None` once the shell closes its end.
    ///
    /// Each call returns one read's worth of bytes and hands ownership to the
    /// caller, so the chunk can move on without a further copy.
    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>>;
    async fn kill(&self) -> io::Result<()>;
}

pub(crate) struct PtyTerminal {
    writer: Mutex<Box<dyn io::Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl PtyTerminal {
    pub(crate) fn new(
        writer: Box<dyn io::Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        reader: Box<dyn io::Read + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);

        std::thread::spawn(move || {
            blocking_read_loop(reader, tx);
        });

        Self {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            read_rx: tokio::sync::Mutex::new(rx),
        }
    }
}

fn blocking_read_loop(mut reader: Box<dyn io::Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if tx.blocking_send(buf[..n].to_vec()).is_err() {
            break;
        }
    }
}

#[async_trait]
impl TerminalHost for PtyTerminal {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        writer.write_all(data)?;
        writer.flush()
    }

    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.read_rx.lock().await.recv().await)
    }

    async fn kill(&self) -> io::Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        child.kill().map_err(io::Error::other)
    }
}
