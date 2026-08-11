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
use std::{io::Write, ops::Range};

/// APC introducer, `ESC _`.
const INTRODUCER: &[u8] = b"\x1b_";
/// String Terminator, `ESC \`.
const TERMINATOR: &[u8] = b"\x1b\\";
/// Bell, accepted as an alternate terminator since intermediaries emit it.
const BEL: u8 = 0x07;
/// The namespace tag claiming the whole stoatty sub-protocol.
const PREFIX: &[u8] = b"Gstoatty";

/// Largest APC payload the terminal's scanner will buffer, in bytes.
///
/// The cap bounds memory against an APC string that never terminates. A payload
/// that reaches it is dropped whole rather than truncated, so a frame over the
/// limit loses all of its content silently. An encoder whose argument could grow
/// past this has to split its work across several frames.
///
/// The budget covers everything between `ESC _` and the terminator, so the
/// `Gstoatty;<sub>;` prefix and the base64 expansion of the argument both count
/// against it.
pub const MAX_APC_PAYLOAD: usize = 64 * 1024;

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

/// How much of the [`MAX_APC_PAYLOAD`] budget a `frame_len`-byte frame spends.
///
/// The cap covers everything between the introducer and the terminator, so an
/// encoder measuring what it just appended has to discount the wrapper. This
/// owns that arithmetic, since the wrapper's size is this module's business.
///
/// Assumes the `ESC \` terminator [`end`] writes, not the `BEL` a decoder also
/// accepts, so it answers for encoder output rather than arbitrary input.
pub(crate) fn payload_len(frame_len: usize) -> usize {
    frame_len.saturating_sub(INTRODUCER.len() + TERMINATOR.len())
}

/// Parse a stoatty frame, or `None` if `bytes` is not a well-formed one.
///
/// Accepts either the full frame or the bare payload a VT parser yields after
/// stripping `ESC _` and the terminator. Returns `None` for anything to ignore:
/// a foreign or absent `Gstoatty` prefix, a missing sub-command, an argument
/// that is not valid base64, or more than [`MAX_FRAME_ARGS`] arguments.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let body = strip_wrapper(bytes);
    let body = body.strip_prefix(PREFIX)?;
    let body = body.strip_prefix(b";")?;

    let mut fields = body.split(|&byte| byte == b';');

    let sub = fields.next().filter(|sub| !sub.is_empty())?;
    let sub = std::str::from_utf8(sub).ok()?.to_owned();

    let mut args = Vec::new();
    for field in fields {
        if args.len() == MAX_FRAME_ARGS {
            return None;
        }
        args.push(STANDARD.decode(field).ok()?);
    }

    Some(Frame { sub, args })
}

/// Arguments a single frame may carry.
///
/// The widest command in the protocol sends four, so the ceiling is double what
/// anything real needs. Both [`decode`] and [`decode_into`] hold to it, so a
/// frame is well-formed or not for every reader alike.
///
/// The sharper reason is [`decode_into`]'s: it fills a [`FrameScratch`] that
/// retains a buffer per argument position for the terminal's lifetime, and a
/// frame is otherwise bounded only by [`MAX_APC_PAYLOAD`], so one 64KB frame of
/// semicolons would pin tens of thousands of buffers for the session.
pub const MAX_FRAME_ARGS: usize = 8;

/// Reusable argument buffers for [`decode_into`], retained across frames.
///
/// Holds one decoded-argument `Vec` per position, at most [`MAX_FRAME_ARGS`] of
/// them. A steady stream of frames grows these once and then decodes into the
/// retained capacity, so the busy path allocates nothing per frame after
/// warm-up.
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
///
/// A frame carrying more than [`MAX_FRAME_ARGS`] arguments is rejected whole
/// rather than truncated, since a command read from a prefix of its fields would
/// be a different command.
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
        // Checked before the growth below, or the frame this rejects has already
        // taken the memory the cap exists to deny it.
        if i == MAX_FRAME_ARGS {
            return None;
        }

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

/// The first complete APC frame span in `bytes`, from `ESC _` through its
/// `ESC \` or `BEL` terminator inclusive, or `None` if no complete span is
/// present yet.
///
/// A range rather than the bytes, so a caller holding the buffer can splice the
/// frame out and keep what surrounds it. That is what a program reading its own
/// stdin needs, where a terminal's reply arrives mixed in with what someone
/// typed. Leading bytes before the introducer are skipped.
pub fn apc_span(bytes: &[u8]) -> Option<Range<usize>> {
    let start = bytes.windows(2).position(|pair| pair == INTRODUCER)?;
    let rest = &bytes[start..];
    let mut i = 2;
    while i < rest.len() {
        if rest[i] == BEL {
            return Some(start..start + i + 1);
        }
        if rest[i] == INTRODUCER[0] && rest.get(i + 1) == Some(&TERMINATOR[1]) {
            return Some(start..start + i + 2);
        }
        i += 1;
    }
    None
}

/// Strip an optional leading `ESC _` and a trailing `ESC \` or `BEL`.
///
/// A base64 argument and a UTF-8 sub-command never contain `ESC` or `BEL`, so
/// the only such bytes are the wrapper, making the strip unambiguous.
/// How many bytes the first frame in `bytes` occupies, or `None` when `bytes`
/// does not open with a complete one.
///
/// For a caller holding a run that starts with a frame and continues with
/// something else, such as a fill batch whose page content follows its open
/// marker. Slicing to this length gives [`decode`] exactly the frame and none
/// of what trails it.
///
/// The introducer has to be at the very start. A frame further in is not the
/// first thing in the run, and reporting it would let a caller read a marker
/// that belongs to whatever came before.
pub fn first_frame_end(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(INTRODUCER) {
        return None;
    }
    let rest = &bytes[INTRODUCER.len()..];
    let at = rest
        .iter()
        .position(|byte| *byte == BEL || *byte == TERMINATOR[0])?;

    match rest[at] {
        BEL => Some(INTRODUCER.len() + at + 1),
        // An ESC that is not the start of a terminator is not one, and a frame
        // body never contains one, so there is no complete frame here.
        _ => match rest.get(at + 1) {
            Some(&byte) if byte == TERMINATOR[1] => Some(INTRODUCER.len() + at + 2),
            _ => None,
        },
    }
}

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
    use super::{decode, decode_into, encode, Frame, FrameScratch, MAX_FRAME_ARGS};

    fn frame(sub: &str, args: &[&[u8]]) -> Frame {
        Frame {
            sub: sub.to_owned(),
            args: args.iter().map(|arg| arg.to_vec()).collect(),
        }
    }

    /// The scratch is retained for the terminal's lifetime, so whatever one
    /// frame grows it to is memory the session never gets back.
    #[test]
    fn a_frame_past_the_arg_cap_is_rejected_and_leaves_the_scratch_small() {
        let mut scratch = FrameScratch::default();
        let flood = frame("border", &vec![b"".as_slice(); 5000]);

        assert_eq!(
            decode_into(&encode(&flood), &mut scratch),
            None,
            "a frame past the cap decodes to nothing"
        );
        assert!(
            scratch.args.len() <= MAX_FRAME_ARGS,
            "the rejected frame grew the scratch no further than the cap, got {}",
            scratch.args.len()
        );
    }

    /// Two entry points that disagree on what a well-formed frame is would let a
    /// caller act on one the terminal's own ingest refuses.
    #[test]
    fn both_decoders_agree_on_the_arg_cap() {
        let mut scratch = FrameScratch::default();
        let full = encode(&frame("border", &[b"x".as_slice(); MAX_FRAME_ARGS]));
        let over = encode(&frame("border", &[b"x".as_slice(); MAX_FRAME_ARGS + 1]));

        assert!(decode(&full).is_some(), "the owning decoder takes the cap");
        assert!(
            decode_into(&full, &mut scratch).is_some(),
            "the borrowing decoder takes it too"
        );

        assert_eq!(decode(&over), None, "the owning decoder rejects past it");
        assert_eq!(
            decode_into(&over, &mut scratch),
            None,
            "and the borrowing decoder agrees"
        );
    }

    /// The widest real command sends four arguments, so the cap has to admit at
    /// least that many, and admitting exactly the cap pins it off by one.
    #[test]
    fn a_frame_at_the_arg_cap_still_decodes() {
        let mut scratch = FrameScratch::default();
        let full = frame("border", &[b"x".as_slice(); MAX_FRAME_ARGS]);

        let encoded = encode(&full);
        let (sub, args) = decode_into(&encoded, &mut scratch).expect("the cap is inclusive");
        assert_eq!((sub, args.len()), ("border", MAX_FRAME_ARGS));
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
