//! The lane markers split the decoration scene into numbered lanes.
//!
//! A `reset` clears every decoration, so an emitter without lanes sends the
//! whole scene again to change one part of it. A lane lets the emitter
//! re-declare only the part that changed.
//!
//! A `lane_begin` frame opens a lane, and a `lane_end` frame closes it. Each
//! decoration between the two frames belongs to that lane, and a decoration
//! outside any lane belongs to lane 0. A `lane_begin` clears the decorations
//! its lane already holds, and the other lanes keep theirs. A bare `reset`
//! still clears every lane.
//!
//! Lanes do not nest. A `lane_begin` inside an open lane opens the new lane in
//! its place.
//!
//! A terminal before protocol version 12 ignores both frames, so it reads a
//! lane's decorations as part of the one scene. Send lanes only to a terminal
//! at version 12 or later.

use crate::frame;

/// Encode a [`Command::LaneBegin`] as a full `Gstoatty;lane_begin` frame for an
/// emitter.
pub fn encode_lane_begin(id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_lane_begin_into(&mut out, id);
    out
}

/// Append a `Gstoatty;lane_begin` frame opening lane `id` to `out`.
///
/// The id rides one 4-byte big-endian argument.
pub fn encode_lane_begin_into(out: &mut Vec<u8>, id: u32) {
    frame::begin(out, "lane_begin");
    frame::push_arg(out, |w| w.write_all(&id.to_be_bytes()));
    frame::end(out);
}

/// Encode a [`Command::LaneEnd`] as a full `Gstoatty;lane_end` frame for an
/// emitter.
pub fn encode_lane_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_lane_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;lane_end` frame to `out`.
pub fn encode_lane_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "lane_end");
    frame::end(out);
}

pub(super) fn decode_lane_begin(args: &[Vec<u8>]) -> Option<u32> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some(u32::from_be_bytes(*arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, decode_stream, encode_bar, BarCommand, Command};

    #[test]
    fn lane_markers_round_trip() {
        assert_eq!(
            encode_lane_begin(0x0102_0304),
            b"\x1b_Gstoatty;lane_begin;AQIDBA==\x1b\\",
            "the id rides big-endian like every other field",
        );
        assert_eq!(
            decode(&encode_lane_begin(0x0102_0304)),
            Some(Command::LaneBegin(0x0102_0304))
        );
        assert_eq!(decode(&encode_lane_end()), Some(Command::LaneEnd));
    }

    #[test]
    fn decode_stream_reads_a_lane_around_its_decorations() {
        let bar = BarCommand {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
            color: [5, 6, 7],
        };
        let mut bytes = encode_lane_begin(3);
        bytes.extend(encode_bar(&bar));
        bytes.extend(encode_lane_end());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::LaneBegin(3), Command::Bar(bar), Command::LaneEnd]
        );
    }

    #[test]
    fn a_lane_begin_without_a_whole_id_is_ignored() {
        // "AAAH" is base64 for three bytes, one short of an id.
        assert_eq!(decode(b"Gstoatty;lane_begin;AAAH"), None);
        assert_eq!(decode(b"Gstoatty;lane_begin"), None, "the id is required");
    }
}
