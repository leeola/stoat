//! Wire contract between a detachable stoat server and the clients that
//! attach to it.
//!
//! A remote stoat run as a server owns its own PTY pair and outlives the link
//! that reached it, so a dropped connection costs the client and not the
//! editing session. A later client attaches to the same socket and the server
//! re-declares its whole terminal state.
//!
//! Only the format lives here. It holds the frames, the socket path, and the
//! rule for a session name. The server and the client both link against it, so
//! neither owns the format.

use snafu::Snafu;
use std::{
    io,
    path::{Path, PathBuf},
};

/// Exit code a client uses after another client displaces it.
///
/// Distinct from an editor exit, so the local side reporting the session knows
/// the remote is still alive and keeps its reconnect target.
pub const REPLACED_EXIT: i32 = 3;

/// What a displaced client prints before exiting with [`REPLACED_EXIT`].
pub const REPLACED_MESSAGE: &str = "replaced by another client";

/// Longest session name [`valid_name`] accepts.
const MAX_NAME: usize = 64;

/// Payload length a [`Frame::Winsize`] always carries, four big-endian u16s.
const WINSIZE_LEN: usize = 8;

const TAG_BYTES: u8 = 0;
const TAG_WINSIZE: u8 = 1;
const TAG_REPLACED: u8 = 2;

/// One message on the attach socket.
///
/// Terminal bytes flow both ways unchanged, which is what lets the server drive
/// a client's screen and the client feed the server its keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Terminal bytes, as they are.
    Bytes(Vec<u8>),
    /// The client's window size, sent to the server on attach and on every
    /// resize.
    ///
    /// The pixel fields ride along because the server's cell metrics come from
    /// them, and an image is transmitted in pixels but placed in cells.
    Winsize {
        rows: u16,
        cols: u16,
        xpixel: u16,
        ypixel: u16,
    },
    /// The server's last word to a client that another client displaced. The
    /// server sends it and closes. The client exits with [`REPLACED_EXIT`].
    Replaced,
}

/// A frame this version does not read, which desynchronizes the stream.
///
/// Both variants mean the peer is not speaking this format, so the two are
/// separated by what a reader learns rather than by what it does about them.
#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum DecodeError {
    #[snafu(display("unknown frame tag {tag}"))]
    BadTag { tag: u8 },
    /// A known tag carrying a payload it never has.
    #[snafu(display("frame tag {tag} cannot carry {len} bytes"))]
    BadLength { tag: u8, len: usize },
}

/// Reassembles frames from a socket read at a time.
///
/// A socket read carries whatever happened to arrive, so a frame arrives split
/// across reads or several arrive in one. Feed every read to [`Self::push`] and
/// drain with [`Self::next_frame`].
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one socket read's bytes to what is buffered.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, or `None` while one is still arriving.
    ///
    /// An error means the stream is desynchronized. The peer wrote a header this
    /// version does not read, so nothing after it is trustworthy. Close the
    /// connection. The buffer is dropped, so a further call reports `None`
    /// rather than the same error forever.
    pub fn next_frame(&mut self) -> Option<Result<Frame, DecodeError>> {
        const HEADER: usize = 5;

        if self.buf.len() < HEADER {
            return None;
        }
        let tag = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < HEADER + len {
            return None;
        }

        let payload = &self.buf[HEADER..HEADER + len];
        let frame = match tag {
            TAG_BYTES => Frame::Bytes(payload.to_vec()),
            TAG_WINSIZE if len == WINSIZE_LEN => Frame::Winsize {
                rows: u16::from_be_bytes([payload[0], payload[1]]),
                cols: u16::from_be_bytes([payload[2], payload[3]]),
                xpixel: u16::from_be_bytes([payload[4], payload[5]]),
                ypixel: u16::from_be_bytes([payload[6], payload[7]]),
            },
            TAG_REPLACED if len == 0 => Frame::Replaced,
            TAG_WINSIZE | TAG_REPLACED => {
                self.buf.clear();
                return Some(Err(DecodeError::BadLength { tag, len }));
            },
            tag => {
                self.buf.clear();
                return Some(Err(DecodeError::BadTag { tag }));
            },
        };

        self.buf.drain(..HEADER + len);
        Some(Ok(frame))
    }
}

/// Append `frame`'s wire form to `out`.
///
/// A tag byte, then the payload length as a big-endian `u32`, then the payload.
/// The length is there for every frame, including the ones whose size the tag
/// already implies, so a reader skips a frame it does not understand rather
/// than needing a table of sizes.
pub fn encode(frame: &Frame, out: &mut Vec<u8>) {
    match frame {
        Frame::Bytes(bytes) => {
            out.push(TAG_BYTES);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        },
        Frame::Winsize {
            rows,
            cols,
            xpixel,
            ypixel,
        } => {
            out.push(TAG_WINSIZE);
            out.extend_from_slice(&(WINSIZE_LEN as u32).to_be_bytes());
            for field in [rows, cols, xpixel, ypixel] {
                out.extend_from_slice(&field.to_be_bytes());
            }
        },
        Frame::Replaced => {
            out.push(TAG_REPLACED);
            out.extend_from_slice(&0u32.to_be_bytes());
        },
    }
}

/// Whether `name` is safe to build a socket path from.
///
/// One to [`MAX_NAME`] characters, each of them ASCII alphanumeric, `-`, or
/// `_`. A separator or a `..` in a name escapes the socket directory, so the
/// rule is a whitelist rather than a list of what to reject.
///
/// Check a name here before it reaches [`socket_path`], which joins whatever it
/// is given.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Socket path for the session called `name`, under the Stoat state dir.
pub fn socket_path(name: &str) -> io::Result<PathBuf> {
    Ok(socket_path_in(&stoat_log::state_dir()?, name))
}

/// Socket path for `name` under `dir`.
///
/// The naming half of [`socket_path`], split out so a caller holding a
/// directory of its own resolves the same name without touching the real
/// environment.
pub fn socket_path_in(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("attach-{name}.sock"))
}

#[cfg(test)]
mod tests {
    use super::{encode, socket_path_in, valid_name, DecodeError, Frame, FrameDecoder, MAX_NAME};
    use std::path::{Path, PathBuf};

    fn round_trip(frame: &Frame) -> Option<Result<Frame, DecodeError>> {
        let mut wire = Vec::new();
        encode(frame, &mut wire);
        let mut decoder = FrameDecoder::new();
        decoder.push(&wire);
        decoder.next_frame()
    }

    #[test]
    fn every_frame_round_trips() {
        for frame in [
            Frame::Bytes(b"\x1b[2J".to_vec()),
            Frame::Bytes(Vec::new()),
            Frame::Winsize {
                rows: 24,
                cols: 80,
                xpixel: 640,
                ypixel: 384,
            },
            Frame::Replaced,
        ] {
            assert_eq!(round_trip(&frame), Some(Ok(frame.clone())), "{frame:?}");
        }
    }

    #[test]
    fn a_frame_split_across_reads_decodes_once_it_is_whole() {
        let frame = Frame::Bytes(b"hello".to_vec());
        let mut wire = Vec::new();
        encode(&frame, &mut wire);

        let mut decoder = FrameDecoder::new();
        let (head, tail) = wire.split_at(3);
        decoder.push(head);
        assert_eq!(decoder.next_frame(), None, "a partial header holds nothing");

        decoder.push(&tail[..tail.len() - 2]);
        assert_eq!(
            decoder.next_frame(),
            None,
            "and a partial payload holds too"
        );

        decoder.push(&tail[tail.len() - 2..]);
        assert_eq!(decoder.next_frame(), Some(Ok(frame)));
        assert_eq!(decoder.next_frame(), None, "the buffer is spent");
    }

    #[test]
    fn back_to_back_frames_drain_in_order() {
        let frames = [Frame::Bytes(b"a".to_vec()), Frame::Replaced];
        let mut wire = Vec::new();
        for frame in &frames {
            encode(frame, &mut wire);
        }

        let mut decoder = FrameDecoder::new();
        decoder.push(&wire);
        let drained: Vec<_> = std::iter::from_fn(|| decoder.next_frame()).collect();
        assert_eq!(drained, frames.iter().cloned().map(Ok).collect::<Vec<_>>());
    }

    #[test]
    fn an_unknown_tag_reports_once_and_drops_the_stream() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&[9, 0, 0, 0, 0, 7, 7, 7]);

        assert_eq!(
            decoder.next_frame(),
            Some(Err(DecodeError::BadTag { tag: 9 })),
            "the tag is not one this version writes",
        );
        assert_eq!(
            decoder.next_frame(),
            None,
            "and the desynchronized remainder is gone rather than erroring forever",
        );
    }

    #[test]
    fn a_known_tag_with_an_impossible_payload_is_told_apart_from_an_unknown_one() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&[1, 0, 0, 0, 2, 0, 0]);

        assert_eq!(
            decoder.next_frame(),
            Some(Err(DecodeError::BadLength { tag: 1, len: 2 })),
            "a winsize never carries two bytes",
        );
        assert_eq!(decoder.next_frame(), None, "and the remainder is dropped");
    }

    #[test]
    fn valid_name_admits_only_what_stays_inside_the_socket_dir() {
        assert!(valid_name("box"));
        assert!(valid_name("a_b-9"));
        assert!(valid_name(&"n".repeat(MAX_NAME)));

        assert!(!valid_name(""), "an empty name builds attach-.sock");
        assert!(!valid_name("../x"), "a traversal escapes the dir");
        assert!(!valid_name("a/b"), "so does a bare separator");
        assert!(!valid_name(&"n".repeat(MAX_NAME + 1)));
    }

    #[test]
    fn socket_path_in_names_the_session_under_the_given_dir() {
        assert_eq!(
            socket_path_in(Path::new("/state"), "box"),
            PathBuf::from("/state/attach-box.sock"),
        );
    }
}
