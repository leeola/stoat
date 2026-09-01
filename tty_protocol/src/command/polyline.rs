//! The polyline command strokes a free path off the cell grid.
//!
//! The protocol's only primitive that is not axis-aligned, added for the commit
//! graph's lane and merge lines.

use crate::frame;

/// A stroked path drawn off the cell grid in a solid color.
///
/// The protocol's only non-axis-aligned primitive, added for the commit
/// graph's lane and merge lines. Every coordinate and [`Self::width`] is in
/// **sixteenths of a cell** like [`BarCommand`], so a path tracks live font
/// zoom.
///
/// A single point, or two equal points, is legal and draws a dot. That is how
/// a graph marks a commit node without a second primitive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PolylineCommand {
    /// Vertices in draw order, each `[x, y]`. Consecutive pairs are the
    /// segments. An empty list draws nothing.
    pub points: Vec<[i16; 2]>,
    /// Stroke thickness, centered on the path, in sixteenths of the cell's
    /// width. Measured against the width on both axes, so a diagonal is as
    /// thick as a vertical and 16 draws exactly one column wide.
    pub width: u16,
    pub color: [u8; 3],
}

/// Encode a [`PolylineCommand`] as a full `Gstoatty;polyline` frame for an
/// emitter.
pub fn encode_polyline(command: &PolylineCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_polyline_into(&mut out, command);
    out
}

/// Append a `Gstoatty;polyline` frame for `command` to `out` without
/// allocating.
///
/// The stroke head and the point list ride in one argument. Width and color
/// come first, then each vertex as a pair of big-endian `i16`s. The points are
/// streamed straight into the base64 sink rather than through an intermediate
/// buffer.
///
/// One frame carries at most 12283 points. The terminal's scanner drops a
/// payload past [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so
/// one point too many loses the whole path silently. Split a longer path into
/// several polylines that repeat the vertex they meet at, which draws as one
/// continuous stroke. Passing more panics in debug and emits a frame the
/// terminal discards in release.
pub fn encode_polyline_into(out: &mut Vec<u8>, command: &PolylineCommand) {
    let start = out.len();

    frame::begin(out, "polyline");
    frame::push_arg(out, |w| {
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.color)?;
        for point in &command.points {
            w.write_all(&point[0].to_be_bytes())?;
            w.write_all(&point[1].to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);

    debug_assert!(
        frame::payload_len(out.len() - start) <= frame::MAX_APC_PAYLOAD,
        "a {}-point polyline overruns the frame cap; split it into paths that \
         share the vertex they meet at",
        command.points.len(),
    );
}

/// Bytes a `polyline` payload spends before its points, holding the width as a
/// big-endian `u16` followed by rgb.
const POLYLINE_HEAD: usize = 5;

pub(super) fn decode_polyline(args: &[Vec<u8>]) -> Option<PolylineCommand> {
    let arg = args.first()?;
    let tail = arg.get(POLYLINE_HEAD..)?;
    if tail.len() % 4 != 0 {
        return None;
    }

    let points = tail
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                i16::from_be_bytes([p[0], p[1]]),
                i16::from_be_bytes([p[2], p[3]]),
            ]
        })
        .collect();
    Some(PolylineCommand {
        points,
        width: u16::from_be_bytes([arg[0], arg[1]]),
        color: [arg[2], arg[3], arg[4]],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command::{decode, Command},
        frame::MAX_APC_PAYLOAD,
    };

    #[test]
    fn polyline_round_trips() {
        let command = PolylineCommand {
            points: vec![[8, 0], [8, 12], [24, 16]],
            width: 6,
            color: [220, 50, 47],
        };

        assert_eq!(
            decode(&encode_polyline(&command)),
            Some(Command::Polyline(command))
        );
    }

    #[test]
    fn a_one_point_polyline_round_trips_as_a_dot() {
        let command = PolylineCommand {
            points: vec![[-4, 40]],
            width: 6,
            color: [1, 2, 3],
        };

        assert_eq!(
            decode(&encode_polyline(&command)),
            Some(Command::Polyline(command))
        );
    }

    #[test]
    fn rejects_a_polyline_payload_with_a_partial_point() {
        // This arg decodes to six bytes, the five-byte head plus one stray,
        // where a point needs four.
        assert!(decode(b"Gstoatty;polyline;YWJjZGVm").is_none());
    }

    #[test]
    fn rejects_a_polyline_payload_shorter_than_its_head() {
        assert!(decode(b"Gstoatty;polyline;YWJj").is_none());
    }

    /// Points a `polyline` frame holds, pinned the same way.
    const POLYLINE_CAP: usize = 12283;

    #[test]
    fn a_polyline_at_the_cap_round_trips() {
        let command = PolylineCommand {
            width: 8,
            color: [1, 2, 3],
            points: vec![[4, 5]; POLYLINE_CAP],
        };
        let encoded = encode_polyline(&command);

        assert!(
            frame::payload_len(encoded.len()) <= MAX_APC_PAYLOAD,
            "the frame at the documented ceiling fits the scanner's budget, got {}",
            frame::payload_len(encoded.len()),
        );
        assert_eq!(decode(&encoded), Some(Command::Polyline(command)));
    }

    #[test]
    #[should_panic(expected = "overruns the frame cap")]
    fn a_polyline_past_the_cap_panics_in_debug() {
        encode_polyline(&PolylineCommand {
            width: 8,
            color: [1, 2, 3],
            points: vec![[4, 5]; POLYLINE_CAP + 1],
        });
    }
}
