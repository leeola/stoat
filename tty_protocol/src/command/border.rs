//! The border command frames a cell region with per-cell perimeter edges.
//!
//! The line-style codes live here rather than with the panel, which draws the
//! same styles and reads them through this module.

use crate::frame;

/// Frame a rectangular cell region with a border.
///
/// The region is `width` by `height` cells with its top-left at (`top`, `left`)
/// in absolute grid coordinates; the terminal sets the matching edge on each
/// perimeter cell. Carried in stoatty_protocol's own types because the crate
/// stays free of the terminal-model dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BorderCommand {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub style: BorderStyle,
    pub color: [u8; 3],
}

/// How a border edge is drawn.
///
/// [`BorderStyle::Light`], [`BorderStyle::Heavy`], and [`BorderStyle::Double`]
/// select the line weight. [`BorderStyle::Rounded`] is a light line whose
/// corners arc where two adjacent edges of the region meet.
///
/// [`BorderStyle::None_`] draws no line. A panel in it with no fill and no
/// shadow draws nothing, yet it covers its rect as every panel does, so pane
/// pool composites and the chrome declared before it leave the rect to the
/// live grid. A program whose frame the cells draw declares such a panel to
/// keep the pools off those cells. A terminal before protocol version 15 reads
/// the style as [`BorderStyle::Light`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderStyle {
    Light,
    Heavy,
    Double,
    Rounded,
    None_,
}

/// Encode a [`BorderCommand`] as a full `Gstoatty;border` frame for an emitter.
pub fn encode_border(command: &BorderCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_border_into(&mut out, command);
    out
}

/// Append a `Gstoatty;border` frame for `command` to `out` without allocating.
pub fn encode_border_into(out: &mut Vec<u8>, command: &BorderCommand) {
    frame::begin(out, "border");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&[style_code(command.style)])?;
        w.write_all(&command.color)
    });
    frame::end(out);
}

pub(super) fn decode_border(args: &[Vec<u8>]) -> Option<BorderCommand> {
    let arg: &[u8; 12] = args.first()?.get(..12)?.try_into().ok()?;

    Some(BorderCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        style: decode_style(arg[8]),
        color: [arg[9], arg[10], arg[11]],
    })
}

/// An unknown code falls back to `Light` rather than killing the command, so a
/// style added later draws in the plainest form an older terminal has instead of
/// making the whole border vanish.
pub(super) fn decode_style(code: u8) -> BorderStyle {
    match code {
        1 => BorderStyle::Heavy,
        2 => BorderStyle::Double,
        3 => BorderStyle::Rounded,
        4 => BorderStyle::None_,
        _ => BorderStyle::Light,
    }
}

pub(super) fn style_code(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::Light => 0,
        BorderStyle::Heavy => 1,
        BorderStyle::Double => 2,
        BorderStyle::Rounded => 3,
        BorderStyle::None_ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn border_round_trips() {
        let command = BorderCommand {
            top: 2,
            left: 40,
            width: 24,
            height: 6,
            style: BorderStyle::Heavy,
            color: [255, 0, 255],
        };

        assert_eq!(
            decode(&encode_border(&command)),
            Some(Command::Border(command))
        );
    }

    #[test]
    fn rounded_style_round_trips() {
        let command = BorderCommand {
            top: 0,
            left: 0,
            width: 4,
            height: 3,
            style: BorderStyle::Rounded,
            color: [1, 2, 3],
        };

        assert_eq!(
            decode(&encode_border(&command)),
            Some(Command::Border(command))
        );
    }

    #[test]
    fn the_style_with_no_line_round_trips() {
        let command = BorderCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            style: BorderStyle::None_,
            color: [1, 2, 3],
        };

        assert_eq!(
            decode(&encode_border(&command)),
            Some(Command::Border(command))
        );
    }

    #[test]
    fn rejects_wrong_length_border_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a border needs.
        assert!(decode(b"Gstoatty;border;YWJj").is_none());
    }
}
