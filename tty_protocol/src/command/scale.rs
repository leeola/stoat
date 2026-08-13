//! The scale command draws a cell's glyph over a block several cells wide.
//!
//! The glyph is whatever the VT stream wrote at that cell, so scale is an
//! attribute applied to existing text rather than a command carrying its own.

use crate::frame;

/// Draw the glyph at a cell `scale` times the cell size.
///
/// The cell at (`top`, `left`) in absolute grid coordinates becomes the
/// top-left of a `scale` by `scale` block the glyph is drawn over; the terminal
/// claims the rest of the block so neighbors do not draw into it. The glyph
/// itself is whatever the VT stream wrote at that cell, so scale is an attribute
/// applied to existing text rather than carrying its own glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScaleCommand {
    pub top: u16,
    pub left: u16,
    pub scale: u8,
}

/// Encode a [`ScaleCommand`] as a full `Gstoatty;scale` frame for an emitter.
pub fn encode_scale(command: &ScaleCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scale_into(&mut out, command);
    out
}

/// Append a `Gstoatty;scale` frame for `command` to `out` without allocating.
pub fn encode_scale_into(out: &mut Vec<u8>, command: &ScaleCommand) {
    frame::begin(out, "scale");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&[command.scale])
    });
    frame::end(out);
}

pub(super) fn decode_scale(args: &[Vec<u8>]) -> Option<ScaleCommand> {
    let arg: &[u8; 5] = args.first()?.get(..5)?.try_into().ok()?;

    Some(ScaleCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        scale: arg[4],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn scale_round_trips() {
        let command = ScaleCommand {
            top: 13,
            left: 4,
            scale: 2,
        };

        assert_eq!(
            decode(&encode_scale(&command)),
            Some(Command::Scale(command))
        );
    }

    #[test]
    fn rejects_wrong_length_scale_payload() {
        // The single arg here decodes to 3 bytes, not the 5 a scale needs.
        assert!(decode(b"Gstoatty;scale;YWJj").is_none());
    }
}
