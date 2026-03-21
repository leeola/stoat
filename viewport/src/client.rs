use crate::protocol::{ToMain, ToMainCodec, ToViewport, ToViewportCodec};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use std::{io, path::Path};
use tokio::net::UnixStream;
use tokio_util::codec::{FramedRead, FramedWrite};

pub struct ViewportClient {
    writer: FramedWrite<tokio::net::unix::OwnedWriteHalf, ToMainCodec>,
    reader: FramedRead<tokio::net::unix::OwnedReadHalf, ToViewportCodec>,
}

impl ViewportClient {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            writer: FramedWrite::new(write_half, ToMainCodec::new()),
            reader: FramedRead::new(read_half, ToViewportCodec::new()),
        })
    }

    pub async fn send(&mut self, msg: ToMain) -> io::Result<()> {
        self.writer.send(msg).await
    }

    pub async fn recv_frame(&mut self) -> io::Result<Option<Bytes>> {
        match self.reader.next().await {
            Some(Ok(ToViewport::Frame(data))) => Ok(Some(data)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}
