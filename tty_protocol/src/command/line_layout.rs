//! The line-layout command declares the row height of every logical line.
//!
//! Off-grid components resolve their declared row against it, so an inline
//! expansion pushes down whatever sits below the line it grows.

use crate::frame;

/// Declare the surface's logical-line layout: the height in rows of each logical
/// line, indexed from the top.
///
/// Most lines are one row; a height greater than one is an integer-cell inline
/// expansion (an inline diff, a multi-line diagnostic) that pushes every later
/// line down. A line past the end of [`Self::heights`] defaults to one row. A
/// non-cell component bound to a logical line reads the prefix sum of these
/// heights to find the physical row it sits on, so it tracks expansions. The
/// full layout is sent on each change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineLayoutCommand {
    pub heights: Vec<u16>,
}

/// Encode a [`LineLayoutCommand`] as a full `Gstoatty;line_layout` frame for an
/// emitter.
///
/// The per-line heights ride in a single argument as consecutive big-endian
/// `u16`s.
pub fn encode_line_layout(command: &LineLayoutCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_line_layout_into(&mut out, &command.heights);
    out
}

/// Append a `Gstoatty;line_layout` frame for `heights` to `out` without
/// allocating.
///
/// `heights` is borrowed and streamed as consecutive big-endian `u16`s straight
/// into the base64 sink, so no intermediate byte buffer is built.
///
/// One frame carries at most 24567 heights. The terminal's scanner drops a
/// payload past [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so
/// one height too many loses the entire layout with nothing on screen to say
/// so. A layout is replaced whole and has no split form, unlike a `minimap_lines`
/// splice, so an emitter with a longer surface has to send the window it
/// declares rather than the whole document. Passing more panics in debug and
/// emits a frame the terminal discards in release.
pub fn encode_line_layout_into(out: &mut Vec<u8>, heights: &[u16]) {
    let start = out.len();

    frame::begin(out, "line_layout");
    frame::push_arg(out, |w| {
        for height in heights {
            w.write_all(&height.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);

    debug_assert!(
        frame::payload_len(out.len() - start) <= frame::MAX_APC_PAYLOAD,
        "a {}-height line_layout overruns the frame cap; declare the visible \
         window instead, since a layout has no split form",
        heights.len(),
    );
}

pub(super) fn decode_line_layout(args: &[Vec<u8>]) -> Option<LineLayoutCommand> {
    let arg = args.first()?;
    if arg.len() % 2 != 0 {
        return None;
    }

    let heights = arg
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    Some(LineLayoutCommand { heights })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command::{decode, Command},
        frame::MAX_APC_PAYLOAD,
    };

    #[test]
    fn line_layout_round_trips() {
        let command = LineLayoutCommand {
            heights: vec![1, 3, 1, 2],
        };

        assert_eq!(
            decode(&encode_line_layout(&command)),
            Some(Command::LineLayout(command))
        );
    }

    #[test]
    fn rejects_odd_length_line_layout_payload() {
        // The single arg here decodes to 3 bytes, not a whole number of u16s.
        assert!(decode(b"Gstoatty;line_layout;YWJj").is_none());
    }

    /// Heights a `line_layout` frame holds. The doc names this ceiling, so the
    /// pair of tests below pins it from both sides.
    const LINE_LAYOUT_CAP: usize = 24567;

    #[test]
    fn a_line_layout_at_the_cap_round_trips() {
        let command = LineLayoutCommand {
            heights: vec![1; LINE_LAYOUT_CAP],
        };
        let encoded = encode_line_layout(&command);

        assert!(
            frame::payload_len(encoded.len()) <= MAX_APC_PAYLOAD,
            "the frame at the documented ceiling fits the scanner's budget, got {}",
            frame::payload_len(encoded.len()),
        );
        assert_eq!(decode(&encoded), Some(Command::LineLayout(command)));
    }

    /// One height past the ceiling. The terminal drops an over-cap payload
    /// whole, so without this the layout vanishes with nothing to say why.
    #[test]
    #[should_panic(expected = "overruns the frame cap")]
    fn a_line_layout_past_the_cap_panics_in_debug() {
        encode_line_layout(&LineLayoutCommand {
            heights: vec![1; LINE_LAYOUT_CAP + 1],
        });
    }
}
