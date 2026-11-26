//! The scrim command dims the window behind the floating boxes of a scene.
//!
//! A modal dialog stands apart from the window it covers because everything
//! behind it darkens. The darkening is one translucent color over the whole
//! picture, which holds the ground, the cells, the decorations, the images,
//! and the pane pools alike. Only the terminal composes that picture, and a
//! program that blends the color into its cells reaches the cell colors alone.
//! A `scrim` frame has the terminal lay the color itself.
//!
//! The scrim covers the surface from `top_px` logical px under the top of the
//! cell row `top_row` down to the bottom, at `alpha` out of 255. It leaves the
//! box of each floating panel clear, which is a panel whose shadow is a drop or
//! a halo, so a dialog and what it holds stay bright. The exterior shadow of
//! such a box falls on the scrim.
//!
//! The frame is a decoration of its lane. A lane declared again without it
//! holds no scrim, and where two lanes hold one, the higher lane's scrim draws.
//! A `reset` clears it with the other decorations.
//!
//! A terminal before protocol version 43 ignores the frame and draws the
//! window undimmed, so send a scrim only to a terminal at
//! [`crate::SCRIM_PROTOCOL`] or later.

use crate::frame;

/// The color, the opacity, and the top of a [`super::Command::Scrim`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrimCommand {
    pub color: [u8; 3],
    /// Out of 255, so zero dims nothing.
    pub alpha: u8,
    /// The cell row whose top [`Self::top_px`] counts from.
    pub top_row: u16,
    /// Logical px from the top of [`Self::top_row`] down to the scrim's top,
    /// negative for a top above that row.
    pub top_px: i16,
}

/// Encode a [`super::Command::Scrim`] as a full `Gstoatty;scrim` frame for an
/// emitter.
pub fn encode_scrim(scrim: &ScrimCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scrim_into(&mut out, scrim);
    out
}

/// Append a `Gstoatty;scrim` frame of `scrim` to `out`.
///
/// The scrim rides one 8-byte argument, the color and the alpha, then the top
/// row and the top px, each big-endian.
pub fn encode_scrim_into(out: &mut Vec<u8>, scrim: &ScrimCommand) {
    frame::begin(out, "scrim");
    frame::push_arg(out, |w| {
        w.write_all(&scrim.color)?;
        w.write_all(&[scrim.alpha])?;
        w.write_all(&scrim.top_row.to_be_bytes())?;
        w.write_all(&scrim.top_px.to_be_bytes())
    });
    frame::end(out);
}

pub(super) fn decode_scrim(args: &[Vec<u8>]) -> Option<ScrimCommand> {
    let arg: &[u8; 8] = args.first()?.get(..8)?.try_into().ok()?;

    Some(ScrimCommand {
        color: [arg[0], arg[1], arg[2]],
        alpha: arg[3],
        top_row: u16::from_be_bytes([arg[4], arg[5]]),
        top_px: i16::from_be_bytes([arg[6], arg[7]]),
    })
}

#[cfg(test)]
mod tests {
    use super::{encode_scrim, ScrimCommand};
    use crate::command::{decode, Command};

    #[test]
    fn scrim_round_trips() {
        let scrim = ScrimCommand {
            color: [8, 9, 11],
            alpha: 115,
            top_row: 2,
            top_px: -4,
        };
        assert_eq!(
            encode_scrim(&scrim),
            b"\x1b_Gstoatty;scrim;CAkLcwAC//w=\x1b\\",
            "the color, the alpha, then the two tops, each big-endian",
        );

        let bounds = ScrimCommand {
            color: [255, 0, 1],
            alpha: 0,
            top_row: u16::MAX,
            top_px: i16::MIN,
        };
        assert_eq!(
            [scrim, bounds].map(|scrim| decode(&encode_scrim(&scrim))),
            [Some(Command::Scrim(scrim)), Some(Command::Scrim(bounds))],
        );
    }

    #[test]
    fn rejects_a_short_scrim_payload() {
        assert_eq!(
            decode(b"Gstoatty;scrim;CAkLcwAC/w=="),
            None,
            "seven bytes, one short of a scrim",
        );
        assert_eq!(decode(b"Gstoatty;scrim"), None, "the payload is required");
    }
}
