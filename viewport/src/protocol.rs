use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

const TAG_FRAME: u8 = 1;

const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_DETACH: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToViewport {
    Frame(Bytes),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToMain {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Detach,
}

pub struct ToViewportCodec {
    inner: LengthDelimitedCodec,
}

impl Default for ToViewportCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ToViewportCodec {
    pub fn new() -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .length_field_type::<u32>()
                .little_endian()
                .new_codec(),
        }
    }
}

impl Encoder<ToViewport> for ToViewportCodec {
    type Error = io::Error;

    fn encode(&mut self, item: ToViewport, dst: &mut BytesMut) -> io::Result<()> {
        let ToViewport::Frame(data) = item;
        let mut buf = BytesMut::with_capacity(1 + data.len());
        buf.put_u8(TAG_FRAME);
        buf.extend_from_slice(&data);
        self.inner.encode(buf.freeze(), dst)
    }
}

impl Decoder for ToViewportCodec {
    type Item = ToViewport;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<ToViewport>> {
        let Some(mut frame) = self.inner.decode(src)? else {
            return Ok(None);
        };
        if frame.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
        }
        let tag = frame.get_u8();
        match tag {
            TAG_FRAME => Ok(Some(ToViewport::Frame(frame.freeze()))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown ToViewport tag: {tag}"),
            )),
        }
    }
}

pub struct ToMainCodec {
    inner: LengthDelimitedCodec,
}

impl Default for ToMainCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ToMainCodec {
    pub fn new() -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .length_field_type::<u32>()
                .little_endian()
                .new_codec(),
        }
    }
}

impl Encoder<ToMain> for ToMainCodec {
    type Error = io::Error;

    fn encode(&mut self, item: ToMain, dst: &mut BytesMut) -> io::Result<()> {
        let mut buf = BytesMut::new();
        match item {
            ToMain::Input(data) => {
                buf.put_u8(TAG_INPUT);
                buf.extend_from_slice(&data);
            },
            ToMain::Resize { cols, rows } => {
                buf.put_u8(TAG_RESIZE);
                buf.put_u16_le(cols);
                buf.put_u16_le(rows);
            },
            ToMain::Detach => {
                buf.put_u8(TAG_DETACH);
            },
        }
        self.inner.encode(buf.freeze(), dst)
    }
}

impl Decoder for ToMainCodec {
    type Item = ToMain;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<ToMain>> {
        let Some(mut frame) = self.inner.decode(src)? else {
            return Ok(None);
        };
        if frame.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
        }
        let tag = frame.get_u8();
        match tag {
            TAG_INPUT => Ok(Some(ToMain::Input(frame.to_vec()))),
            TAG_RESIZE => {
                if frame.remaining() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Resize frame too short",
                    ));
                }
                let cols = frame.get_u16_le();
                let rows = frame.get_u16_le();
                Ok(Some(ToMain::Resize { cols, rows }))
            },
            TAG_DETACH => Ok(Some(ToMain::Detach)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown ToMain tag: {tag}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_frame() {
        let mut codec = ToViewportCodec::new();
        let original = ToViewport::Frame(Bytes::from_static(b"\x1b[31mhello\x1b[0m"));

        let mut buf = BytesMut::new();
        codec.encode(original.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_input() {
        let mut codec = ToMainCodec::new();
        let original = ToMain::Input(vec![0x1b, 0x5b, 0x41]);

        let mut buf = BytesMut::new();
        codec.encode(original.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_resize() {
        let mut codec = ToMainCodec::new();
        let original = ToMain::Resize {
            cols: 120,
            rows: 40,
        };

        let mut buf = BytesMut::new();
        codec.encode(original.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_detach() {
        let mut codec = ToMainCodec::new();
        let original = ToMain::Detach;

        let mut buf = BytesMut::new();
        codec.encode(original.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn multiple_messages_in_buffer() {
        let mut codec = ToMainCodec::new();
        let msgs = vec![
            ToMain::Input(vec![b'a']),
            ToMain::Resize { cols: 80, rows: 24 },
            ToMain::Detach,
        ];

        let mut buf = BytesMut::new();
        for msg in &msgs {
            codec.encode(msg.clone(), &mut buf).unwrap();
        }

        for expected in msgs {
            let decoded = codec.decode(&mut buf).unwrap().unwrap();
            assert_eq!(decoded, expected);
        }
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn empty_frame_is_error() {
        let mut codec = ToViewportCodec::new();
        let mut inner = LengthDelimitedCodec::builder()
            .length_field_type::<u32>()
            .little_endian()
            .new_codec();

        let mut buf = BytesMut::new();
        inner.encode(Bytes::new(), &mut buf).unwrap();

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_tag_is_error() {
        let mut codec = ToMainCodec::new();
        let mut inner = LengthDelimitedCodec::builder()
            .length_field_type::<u32>()
            .little_endian()
            .new_codec();

        let mut buf = BytesMut::new();
        inner
            .encode(Bytes::from_static(&[0xFF, 0x00]), &mut buf)
            .unwrap();

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }
}
