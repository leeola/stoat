//! The `Gstoatty` APC frame: the wire grammar stoatty programs emit and the
//! terminal decodes.
//!
//! A frame is `ESC _ Gstoatty ; <sub> ; <arg>... ESC \`, an APC string any VT
//! terminal consumes and ignores, so stoatty bytes degrade to nothing
//! elsewhere. `<sub>` names a sub-command; each `<arg>` is base64 so arbitrary
//! binary payloads survive the text stream. [`encode`] produces the full frame
//! for an emitter; [`decode`] parses one back, tolerating either the full frame
//! or the bare payload a parser hands over after stripping `ESC _` and the
//! terminator.

use base64::{engine::general_purpose::STANDARD, write::EncoderWriter, Engine};
use std::io::Write;

/// APC introducer, `ESC _`.
const INTRODUCER: &[u8] = b"\x1b_";
/// String Terminator, `ESC \`.
const TERMINATOR: &[u8] = b"\x1b\\";
/// Bell, accepted as an alternate terminator since intermediaries emit it.
const BEL: u8 = 0x07;
/// The namespace tag claiming the whole stoatty sub-protocol.
const PREFIX: &[u8] = b"Gstoatty";

/// A parsed stoatty frame: a sub-command and its decoded binary arguments.
///
/// `args` holds the raw bytes of each argument after base64 decoding, in
/// emission order; the sub-command decides how many it expects and what they
/// mean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub sub: String,
    pub args: Vec<Vec<u8>>,
}

/// Encode `frame` as the full `ESC _ Gstoatty ; sub ; b64(arg)... ESC \` bytes.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(&mut out, frame);
    out
}

/// Append the full `ESC _ Gstoatty ; sub ; b64(arg)... ESC \` frame to `out`.
///
/// Allocation-free given spare capacity in `out`: each argument's base64 streams
/// straight into the buffer rather than through a per-argument `String`. An
/// emitter re-sending its whole scene each frame reuses one buffer across frames.
pub fn encode_into(out: &mut Vec<u8>, frame: &Frame) {
    begin(out, &frame.sub);
    for arg in &frame.args {
        push_arg(out, |w| w.write_all(arg));
    }
    end(out);
}

/// Write the frame header `ESC _ Gstoatty ; sub` into `out`.
pub(crate) fn begin(out: &mut Vec<u8>, sub: &str) {
    out.extend_from_slice(INTRODUCER);
    out.extend_from_slice(PREFIX);
    out.push(b';');
    out.extend_from_slice(sub.as_bytes());
}

/// Append `; b64(payload)` to `out`, where `payload` is whatever `write_payload`
/// writes into the supplied base64 sink.
///
/// The payload is written through a streaming base64 encoder, so a caller assembling
/// a multi-field argument never materializes it as an intermediate buffer.
pub(crate) fn push_arg(
    out: &mut Vec<u8>,
    write_payload: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
) {
    out.push(b';');
    let mut encoder = EncoderWriter::new(&mut *out, &STANDARD);
    write_payload(&mut encoder).expect("writing to a Vec is infallible");
    encoder.finish().expect("writing to a Vec is infallible");
}

/// Write the frame terminator `ESC \` into `out`.
pub(crate) fn end(out: &mut Vec<u8>) {
    out.extend_from_slice(TERMINATOR);
}

/// Parse a stoatty frame, or `None` if `bytes` is not a well-formed one.
///
/// Accepts either the full frame or the bare payload a VT parser yields after
/// stripping `ESC _` and the terminator. Returns `None` for anything to ignore:
/// a foreign or absent `Gstoatty` prefix, a missing sub-command, or an argument
/// that is not valid base64.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let body = strip_wrapper(bytes);
    let body = body.strip_prefix(PREFIX)?;
    let body = body.strip_prefix(b";")?;

    let mut fields = body.split(|&byte| byte == b';');

    let sub = fields.next().filter(|sub| !sub.is_empty())?;
    let sub = std::str::from_utf8(sub).ok()?.to_owned();

    let mut args = Vec::new();
    for field in fields {
        args.push(STANDARD.decode(field).ok()?);
    }

    Some(Frame { sub, args })
}

/// Reusable argument buffers for [`decode_into`], retained across frames.
///
/// Holds one decoded-argument `Vec` per position. A steady stream of frames
/// grows these once and then decodes into the retained capacity, so the busy
/// path allocates nothing per frame after warm-up.
#[derive(Default)]
pub struct FrameScratch {
    args: Vec<Vec<u8>>,
}

/// Parse a stoatty frame into `scratch`, borrowing the sub-command and decoded
/// arguments out of it, or `None` if `bytes` is not a well-formed frame.
///
/// Like [`decode`] but allocation-free once `scratch` is warm: the sub-command
/// is borrowed from `bytes` rather than owned, and each argument decodes into a
/// retained buffer instead of a fresh `Vec`. The returned slice borrows
/// `scratch`, so it is valid only until the next call reusing the same scratch.
pub fn decode_into<'a>(
    bytes: &'a [u8],
    scratch: &'a mut FrameScratch,
) -> Option<(&'a str, &'a [Vec<u8>])> {
    let body = strip_wrapper(bytes);
    let body = body.strip_prefix(PREFIX)?;
    let body = body.strip_prefix(b";")?;

    let mut fields = body.split(|&byte| byte == b';');

    let sub = fields.next().filter(|sub| !sub.is_empty())?;
    let sub = std::str::from_utf8(sub).ok()?;

    let mut count = 0;
    for (i, field) in fields.enumerate() {
        if i == scratch.args.len() {
            scratch.args.push(Vec::new());
        } else {
            scratch.args[i].clear();
        }

        STANDARD.decode_vec(field, &mut scratch.args[i]).ok()?;
        count = i + 1;
    }

    Some((sub, &scratch.args[..count]))
}

/// Strip an optional leading `ESC _` and a trailing `ESC \` or `BEL`.
///
/// A base64 argument and a UTF-8 sub-command never contain `ESC` or `BEL`, so
/// the only such bytes are the wrapper, making the strip unambiguous.
fn strip_wrapper(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_prefix(INTRODUCER).unwrap_or(bytes);

    if let Some(body) = bytes.strip_suffix(TERMINATOR) {
        body
    } else if let Some((&BEL, body)) = bytes.split_last() {
        body
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, Frame};

    fn frame(sub: &str, args: &[&[u8]]) -> Frame {
        Frame {
            sub: sub.to_owned(),
            args: args.iter().map(|arg| arg.to_vec()).collect(),
        }
    }

    #[test]
    fn round_trips_without_args() {
        let original = frame("border", &[]);
        assert_eq!(decode(&encode(&original)), Some(original));
    }

    #[test]
    fn round_trips_binary_args() {
        let original = frame("scale", &[&[0, 1, 2, 255], b"x"]);
        assert_eq!(decode(&encode(&original)), Some(original));
    }

    #[test]
    fn encode_wraps_payload_in_apc() {
        assert_eq!(encode(&frame("border", &[])), b"\x1b_Gstoatty;border\x1b\\");
    }

    #[test]
    fn decode_accepts_bare_payload() {
        assert_eq!(decode(b"Gstoatty;border"), Some(frame("border", &[])));
    }

    #[test]
    fn decode_accepts_bel_terminator() {
        assert_eq!(
            decode(b"\x1b_Gstoatty;border\x07"),
            Some(frame("border", &[]))
        );
    }

    #[test]
    fn decode_rejects_foreign_prefix() {
        assert_eq!(decode(b"Gkitty;border"), None);
    }

    #[test]
    fn decode_rejects_missing_subcommand() {
        assert_eq!(decode(b"Gstoatty"), None);
        assert_eq!(decode(b"Gstoatty;"), None);
    }

    #[test]
    fn decode_rejects_invalid_base64_arg() {
        assert_eq!(decode(b"Gstoatty;border;@@@"), None);
    }
}
