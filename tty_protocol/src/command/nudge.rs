//! The nudge command offsets the components after it by whole logical pixels.
//!
//! The wire places a text run and a bar in sixteenths of a cell, and a panel on
//! whole cells. A design that sets an element a fixed number of pixels from its
//! row states no such place, since a sixteenth of an 18 pixel row is 1.125
//! pixels. A `nudge` frame gives the text runs, bars, and panels after it an
//! offset in logical pixels, so each one draws that far from its declared place
//! at any cell size.
//!
//! The offset holds for every later `text_run`, `text_runs` entry, `bar`, `bars`
//! entry, and `panel`, until the next `nudge`, `lane_begin`, `lane_end`, or
//! `reset`. An icon takes none, since it carries its own offset.
//!
//! A terminal before protocol version 35 ignores the frame, so it draws each
//! component at its declared place. Send a nudge only to a terminal at version
//! 35 or later.

use crate::frame;

/// Encode a [`super::Command::Nudge`] as a full `Gstoatty;nudge` frame for an
/// emitter.
pub fn encode_nudge(offset: [i16; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_nudge_into(&mut out, offset);
    out
}

/// Append a `Gstoatty;nudge` frame of `offset` to `out`.
///
/// The offset rides one 4-byte argument, dx then dy, each big-endian.
pub fn encode_nudge_into(out: &mut Vec<u8>, [dx, dy]: [i16; 2]) {
    frame::begin(out, "nudge");
    frame::push_arg(out, |w| {
        w.write_all(&dx.to_be_bytes())?;
        w.write_all(&dy.to_be_bytes())
    });
    frame::end(out);
}

pub(super) fn decode_nudge(args: &[Vec<u8>]) -> Option<[i16; 2]> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some([
        i16::from_be_bytes([arg[0], arg[1]]),
        i16::from_be_bytes([arg[2], arg[3]]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn a_nudge_round_trips_both_signs() {
        assert_eq!(
            encode_nudge([1, -2]),
            b"\x1b_Gstoatty;nudge;AAH//g==\x1b\\",
            "dx then dy, each big-endian",
        );
        assert_eq!(
            [[7, -3], [i16::MIN, i16::MAX]].map(|offset| decode(&encode_nudge(offset))),
            [
                Some(Command::Nudge([7, -3])),
                Some(Command::Nudge([i16::MIN, i16::MAX])),
            ],
        );
    }

    #[test]
    fn a_nudge_without_a_whole_offset_is_ignored() {
        // "AAAH" is base64 for three bytes, one short of an offset.
        assert_eq!(decode(b"Gstoatty;nudge;AAAH"), None);
        assert_eq!(decode(b"Gstoatty;nudge"), None, "the offset is required");
    }
}
