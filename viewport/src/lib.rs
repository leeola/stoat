pub mod client;
pub mod listener;
pub mod protocol;
pub mod socket_path;

pub use client::ViewportClient;
pub use listener::{ViewportConnection, ViewportListener};
pub use protocol::{ToMain, ToViewport};
pub use socket_path::socket_path;

#[cfg(test)]
mod tests {
    use crate::{
        protocol::{ToMain, ToMainCodec, ToViewport, ToViewportCodec},
        ViewportListener,
    };
    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::{FramedRead, FramedWrite};

    #[tokio::test]
    async fn frame_round_trip_over_socket_pair() {
        let (a, b) = UnixStream::pair().unwrap();
        let payload = Bytes::from_static(b"\x1b[31mhello\x1b[0m");

        let mut writer = FramedWrite::new(a, ToViewportCodec::new());
        let mut reader = FramedRead::new(b, ToViewportCodec::new());

        writer
            .send(ToViewport::Frame(payload.clone()))
            .await
            .unwrap();
        drop(writer);

        let msg = reader.next().await.unwrap().unwrap();
        assert_eq!(msg, ToViewport::Frame(payload));
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn input_round_trip_over_socket_pair() {
        let (a, b) = UnixStream::pair().unwrap();
        let input = vec![0x1b, 0x5b, 0x41];

        let mut writer = FramedWrite::new(a, ToMainCodec::new());
        let mut reader = FramedRead::new(b, ToMainCodec::new());

        writer.send(ToMain::Input(input.clone())).await.unwrap();
        writer
            .send(ToMain::Resize {
                cols: 120,
                rows: 40,
            })
            .await
            .unwrap();
        writer.send(ToMain::Detach).await.unwrap();
        drop(writer);

        assert_eq!(reader.next().await.unwrap().unwrap(), ToMain::Input(input));
        assert_eq!(
            reader.next().await.unwrap().unwrap(),
            ToMain::Resize {
                cols: 120,
                rows: 40
            }
        );
        assert_eq!(reader.next().await.unwrap().unwrap(), ToMain::Detach);
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn listener_and_client_exchange() {
        let dir = std::env::temp_dir().join(format!("viewport-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");

        let listener = ViewportListener::bind(&sock).await.unwrap();

        let client_handle = tokio::spawn(async move {
            let mut client = crate::ViewportClient::connect(&sock).await.unwrap();
            client.send(ToMain::Input(b"hello".to_vec())).await.unwrap();
            client
                .send(ToMain::Resize { cols: 80, rows: 24 })
                .await
                .unwrap();

            let frame = client.recv_frame().await.unwrap().unwrap();
            assert_eq!(frame, Bytes::from_static(b"response"));

            client.send(ToMain::Detach).await.unwrap();
        });

        let mut conn = listener.accept().await.unwrap();

        let msg = conn.recv().await.unwrap().unwrap();
        assert_eq!(msg, ToMain::Input(b"hello".to_vec()));

        let msg = conn.recv().await.unwrap().unwrap();
        assert_eq!(msg, ToMain::Resize { cols: 80, rows: 24 });

        conn.send_frame(Bytes::from_static(b"response"))
            .await
            .unwrap();

        let msg = conn.recv().await.unwrap().unwrap();
        assert_eq!(msg, ToMain::Detach);

        client_handle.await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn disconnect_yields_none() {
        let dir = std::env::temp_dir().join(format!("viewport-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");

        let listener = ViewportListener::bind(&sock).await.unwrap();

        let client_handle = tokio::spawn(async move {
            let client = crate::ViewportClient::connect(&sock).await.unwrap();
            drop(client);
        });

        let mut conn = listener.accept().await.unwrap();
        client_handle.await.unwrap();

        let msg = conn.recv().await.unwrap();
        assert_eq!(msg, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn many_frames_no_corruption() {
        let (a, b) = UnixStream::pair().unwrap();

        let count: usize = 1000;

        let write_handle = tokio::spawn(async move {
            let mut writer = FramedWrite::new(a, ToViewportCodec::new());
            for i in 0..count {
                let data = format!("frame-{i}").into_bytes();
                writer
                    .send(ToViewport::Frame(Bytes::from(data)))
                    .await
                    .unwrap();
            }
        });

        let mut reader = FramedRead::new(b, ToViewportCodec::new());
        for i in 0..count {
            let expected = format!("frame-{i}").into_bytes();
            let msg = reader.next().await.unwrap().unwrap();
            assert_eq!(msg, ToViewport::Frame(Bytes::from(expected)));
        }

        write_handle.await.unwrap();
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn stale_socket_cleanup() {
        let dir = std::env::temp_dir().join(format!("viewport-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");

        // Create a stale socket file (just a regular file, not a real listener)
        std::fs::write(&sock, b"").unwrap();

        // bind should clean up the stale file and succeed
        let _listener = ViewportListener::bind(&sock).await.unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
