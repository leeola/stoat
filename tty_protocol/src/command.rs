//! The typed command surface: a parsed [`Frame`] dispatched by its namespaced
//! sub-command.
//!
//! [`decode`] is the terminal-facing entry point. It returns `None` for any
//! frame the terminal should ignore, whether malformed or carrying a
//! sub-command this build does not recognize, so an unsupported feature
//! degrades to nothing rather than erroring.

use crate::frame::{self, Frame};

/// A decoded stoatty command.
///
/// Feature sub-codes (scaled text, popovers) add their variants here as those
/// items land. The enum is intentionally exhaustive: adding a variant forces
/// every matcher, including the terminal's apply seam, to handle it rather than
/// silently dropping the new command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Border(BorderCommand),
}

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderStyle {
    Light,
    Heavy,
    Double,
    Rounded,
}

/// Decode a stoatty APC frame into a typed [`Command`], or `None` to ignore it.
///
/// `None` covers both a malformed frame and a well-formed one whose
/// sub-command is unknown to this build. Ignoring rather than erroring is what
/// lets the same byte stream degrade to nothing in another terminal.
pub fn decode(bytes: &[u8]) -> Option<Command> {
    let frame = frame::decode(bytes)?;
    dispatch(&frame)
}

/// Encode a [`BorderCommand`] as a full `Gstoatty;border` frame for an emitter.
pub fn encode_border(command: &BorderCommand) -> Vec<u8> {
    let mut arg = Vec::with_capacity(12);
    arg.extend_from_slice(&command.top.to_be_bytes());
    arg.extend_from_slice(&command.left.to_be_bytes());
    arg.extend_from_slice(&command.width.to_be_bytes());
    arg.extend_from_slice(&command.height.to_be_bytes());
    arg.push(style_code(command.style));
    arg.extend_from_slice(&command.color);

    frame::encode(&Frame {
        sub: "border".to_owned(),
        args: vec![arg],
    })
}

/// Map a parsed [`Frame`] to its [`Command`] by sub-command name.
///
/// An unknown sub-command, or a known one whose payload does not parse, yields
/// `None` so the frame is ignored.
fn dispatch(frame: &Frame) -> Option<Command> {
    match frame.sub.as_str() {
        "border" => decode_border(&frame.args).map(Command::Border),
        _ => None,
    }
}

fn decode_border(args: &[Vec<u8>]) -> Option<BorderCommand> {
    let arg: &[u8; 12] = args.first()?.as_slice().try_into().ok()?;

    Some(BorderCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        style: decode_style(arg[8])?,
        color: [arg[9], arg[10], arg[11]],
    })
}

fn decode_style(code: u8) -> Option<BorderStyle> {
    match code {
        0 => Some(BorderStyle::Light),
        1 => Some(BorderStyle::Heavy),
        2 => Some(BorderStyle::Double),
        3 => Some(BorderStyle::Rounded),
        _ => None,
    }
}

fn style_code(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::Light => 0,
        BorderStyle::Heavy => 1,
        BorderStyle::Double => 2,
        BorderStyle::Rounded => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode_border, BorderCommand, BorderStyle, Command};

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
    fn rejects_wrong_length_border_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a border needs.
        assert!(decode(b"Gstoatty;border;YWJj").is_none());
    }

    #[test]
    fn ignores_unknown_subcommand() {
        assert!(decode(b"Gstoatty;nope").is_none());
    }

    #[test]
    fn ignores_malformed_frame() {
        assert!(decode(b"garbage").is_none());
    }
}
