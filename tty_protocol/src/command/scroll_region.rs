//! The scroll-region command declares a sub-rectangle whose scroll the terminal
//! eases.
//!
//! The program reports an absolute row offset and the terminal animates toward
//! it, so the rest of the grid scrolls independently of the region.

use crate::frame;

/// Declare a scrollable sub-rectangle of the grid.
///
/// The region is `width` by `height` cells with its top-left at (`top`, `left`)
/// in absolute grid coordinates. `offset` is its current scroll position in
/// rows: the renderer eases the region's content as `offset` changes between
/// frames, so the program reports an absolute position and the terminal owns the
/// animation. The rest of the grid scrolls independently of the region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollRegionCommand {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub offset: u16,
}

/// Encode a [`ScrollRegionCommand`] as a full `Gstoatty;scroll_region` frame for
/// an emitter.
pub fn encode_scroll_region(command: &ScrollRegionCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scroll_region_into(&mut out, command);
    out
}

/// Append a `Gstoatty;scroll_region` frame for `command` to `out` without
/// allocating.
pub fn encode_scroll_region_into(out: &mut Vec<u8>, command: &ScrollRegionCommand) {
    frame::begin(out, "scroll_region");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.offset.to_be_bytes())
    });
    frame::end(out);
}

pub(super) fn decode_scroll_region(args: &[Vec<u8>]) -> Option<ScrollRegionCommand> {
    let arg: &[u8; 10] = args.first()?.get(..10)?.try_into().ok()?;

    Some(ScrollRegionCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        offset: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn scroll_region_round_trips() {
        let command = ScrollRegionCommand {
            top: 1,
            left: 60,
            width: 40,
            height: 30,
            offset: 12,
        };

        assert_eq!(
            decode(&encode_scroll_region(&command)),
            Some(Command::ScrollRegion(command))
        );
    }

    #[test]
    fn rejects_wrong_length_scroll_region_payload() {
        // The single arg here decodes to 3 bytes, not the 10 a scroll region needs.
        assert!(decode(b"Gstoatty;scroll_region;YWJj").is_none());
    }
}
