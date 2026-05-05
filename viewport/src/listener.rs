use crate::protocol::{ToMain, ToMainCodec, ToViewport, ToViewportCodec};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_host::FsHost;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{FramedRead, FramedWrite};

pub struct ViewportListener {
    listener: UnixListener,
    path: PathBuf,
    fs: Arc<dyn FsHost>,
}

impl ViewportListener {
    /// Binds a Unix listener at `path`, cleaning up stale sockets via `fs`.
    pub async fn bind(fs: Arc<dyn FsHost>, path: &Path) -> io::Result<Self> {
        if fs.exists(path) {
            match UnixStream::connect(path).await {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "socket already in use by another process",
                    ));
                },
                Err(_) => {
                    fs.remove_file(path)?;
                },
            }
        }
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_owned(),
            fs,
        })
    }

    pub async fn accept(&self) -> io::Result<ViewportConnection> {
        let (stream, _addr) = self.listener.accept().await?;
        Ok(ViewportConnection::new(stream))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ViewportListener {
    fn drop(&mut self) {
        let _ = self.fs.remove_file(&self.path);
    }
}

pub struct ViewportConnection {
    writer: FramedWrite<tokio::net::unix::OwnedWriteHalf, ToViewportCodec>,
    reader: FramedRead<tokio::net::unix::OwnedReadHalf, ToMainCodec>,
}

impl ViewportConnection {
    fn new(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            writer: FramedWrite::new(write_half, ToViewportCodec::new()),
            reader: FramedRead::new(read_half, ToMainCodec::new()),
        }
    }

    pub async fn send_frame(&mut self, frame: Bytes) -> io::Result<()> {
        self.writer.send(ToViewport::Frame(frame)).await
    }

    pub async fn recv(&mut self) -> io::Result<Option<ToMain>> {
        match self.reader.next().await {
            Some(Ok(msg)) => Ok(Some(msg)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}
