//! The bar command fills a thin rectangle off the cell grid in a solid color.
//!
//! A non-cell component packs several of them into a fraction of a cell, so a
//! gutter draws its status and git marks without spending a column on each.

use crate::frame;

/// Fill a thin rectangle off the cell grid in a solid color.
///
/// A non-cell component primitive: a gutter packs several variable-width status
/// or git bars and a hairline separator into a fraction of a cell. All four of
/// [`Self::x`], [`Self::y`], [`Self::width`], and [`Self::height`] are in
/// **sixteenths of a cell** (16 = one cell), x and width along the cell width, y
/// and height along the cell height, so a bar can be a fraction of a cell wide
/// and track live font zoom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarCommand {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: [u8; 3],
}

/// Encode a [`BarCommand`] as a full `Gstoatty;bar` frame for an emitter.
///
/// The position, size, and color ride in a single fixed 11-byte argument.
pub fn encode_bar(command: &BarCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bar_into(&mut out, command);
    out
}

/// Append a `Gstoatty;bar` frame for `command` to `out` without allocating.
pub fn encode_bar_into(out: &mut Vec<u8>, command: &BarCommand) {
    frame::begin(out, "bar");
    frame::push_arg(out, |w| {
        w.write_all(&command.x.to_be_bytes())?;
        w.write_all(&command.y.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.color)
    });
    frame::end(out);
}

pub(super) fn decode_bar(args: &[Vec<u8>]) -> Option<BarCommand> {
    let arg: &[u8; 11] = args.first()?.get(..11)?.try_into().ok()?;

    Some(BarCommand {
        x: i16::from_be_bytes([arg[0], arg[1]]),
        y: i16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        color: [arg[8], arg[9], arg[10]],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn bar_round_trips() {
        let command = BarCommand {
            x: -4,
            y: 32,
            width: 3,
            height: 16,
            color: [220, 50, 47],
        };

        assert_eq!(decode(&encode_bar(&command)), Some(Command::Bar(command)));
    }

    #[test]
    fn rejects_wrong_length_bar_payload() {
        // The single arg here decodes to 3 bytes, not the 11 a bar needs.
        assert!(decode(b"Gstoatty;bar;YWJj").is_none());
    }
}
