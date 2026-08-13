//! The popover command draws a floating box above the grid.
//!
//! Its content streams between the open and close markers rather than riding in
//! the head, so a line of text longer than one argument still arrives whole.

use crate::frame;

/// Draw a floating popover region above the grid.
///
/// A `width` by `height` cell box anchored at (`top`, `left`) in absolute grid
/// coordinates, filled with `fill` and outlined with `border`. The region floats
/// above the cells with its own z-order.
///
/// `content` is a line of text drawn inside the box in `content_fg`, drawn at
/// `scale` times the cell size from the box's top-left, clipped to the box.
///
/// The content is generic over its container so an emitter re-declaring a
/// popover every frame can hand a borrowed slice, while a decoded command owns
/// its copy. It defaults to the owned form, so a holder that is not encoding
/// writes the type with no parameter. A scope encoder takes its content from a
/// closure instead and reads only the head fields, so `PopoverCommand<()>`
/// names a head with no content of its own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PopoverCommand<S = String> {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub fill: [u8; 3],
    pub border: [u8; 3],
    pub content_fg: [u8; 3],
    /// Integer multiple of the cell size the content text is drawn at, so a
    /// tooltip can render larger or smaller than the grid. A scale of 1 matches
    /// the grid metrics.
    pub scale: u8,
    /// Signed `[x, y]` pixel offset from the anchor cell, so a tooltip can sit
    /// exactly under a span rather than snapping to the cell grid. The box, its
    /// shadow, its content, and the content clip all shift by this offset.
    pub offset: [i16; 2],
    /// Shape the content text at bold weight rather than the default. Only the
    /// content is affected. The box chrome is unchanged.
    pub bold: bool,
    pub content: S,
}

/// Encode a [`PopoverCommand`] as a full `Gstoatty;popover` frame for an emitter.
///
/// The region, colors, scale, offset, and bold flag ride in a fixed 23-byte
/// argument on the open marker. The content streams between the markers.
pub fn encode_popover<S: AsRef<str>>(command: &PopoverCommand<S>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_popover_into(&mut out, command);
    out
}

/// Append a `Gstoatty;popover` open marker, its streamed `content`, and a
/// `Gstoatty;popover_end` close marker to `out`.
///
/// The fixed head fields ride in the open marker's single argument, and
/// `command.content` streams as the raw bytes between the two markers, so it is
/// not bounded by the per-frame size cap. The content's container is generic, so
/// an emitter passes a slice of its own buffer rather than building an owned
/// [`String`] per frame.
///
/// The content must be plain text. Riding outside the APC wrapper is what frees
/// it from the size cap and is also what makes it indistinguishable from
/// terminal control bytes, so the terminal cuts the capture at the first `ESC`
/// and keeps only what came before. An emitter that wants styled content sets
/// the colors in the head fields rather than with escape sequences in the text.
pub fn encode_popover_into<S: AsRef<str>>(out: &mut Vec<u8>, command: &PopoverCommand<S>) {
    let content = command.content.as_ref();
    encode_popover_scope(out, command, |out| {
        out.extend_from_slice(content.as_bytes())
    });
}

/// Append a whole popover to `out`, being the open marker, whatever `content`
/// writes, then the close marker.
///
/// The streaming form of [`encode_popover_into`], for a caller that formats its
/// text into the output buffer rather than holding it as one string. Both emit
/// the same bytes and neither has a way to leave the close marker out, which
/// matters because an unclosed popover keeps capturing until the next marker
/// and swallows whatever the emitter writes next.
///
/// Only `command`'s head fields are read, so its content container is
/// unconstrained. Write `PopoverCommand<()>` with `content: ()` to say the
/// closure supplies the text.
///
/// The plain-text rule applies here too. The terminal cuts the capture at the
/// first `ESC`, so `content` must write text and set its colors through the
/// head fields.
pub fn encode_popover_scope<S>(
    out: &mut Vec<u8>,
    command: &PopoverCommand<S>,
    content: impl FnOnce(&mut Vec<u8>),
) {
    frame::begin(out, "popover");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.fill)?;
        w.write_all(&command.border)?;
        w.write_all(&command.content_fg)?;
        w.write_all(&[command.scale])?;
        w.write_all(&command.offset[0].to_be_bytes())?;
        w.write_all(&command.offset[1].to_be_bytes())?;
        w.write_all(&[command.bold as u8])
    });
    frame::end(out);
    content(out);
    encode_popover_end_into(out);
}

/// Encode a [`Command::PopoverEnd`] as a full `Gstoatty;popover_end` close-marker
/// frame.
///
/// The frame carries no arguments; receiving it commits the content streamed
/// since the matching [`Command::Popover`] into the popover's `content`.
pub fn encode_popover_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_popover_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;popover_end` close-marker frame to `out`.
pub fn encode_popover_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "popover_end");
    frame::end(out);
}

/// Decode a `Gstoatty;popover` open marker's head. The `content` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::PopoverEnd`], so it is empty here.
pub(super) fn decode_popover(args: &[Vec<u8>]) -> Option<PopoverCommand> {
    let region: &[u8; 23] = args.first()?.get(..23)?.try_into().ok()?;

    Some(PopoverCommand {
        top: u16::from_be_bytes([region[0], region[1]]),
        left: u16::from_be_bytes([region[2], region[3]]),
        width: u16::from_be_bytes([region[4], region[5]]),
        height: u16::from_be_bytes([region[6], region[7]]),
        fill: [region[8], region[9], region[10]],
        border: [region[11], region[12], region[13]],
        content_fg: [region[14], region[15], region[16]],
        scale: region[17],
        offset: [
            i16::from_be_bytes([region[18], region[19]]),
            i16::from_be_bytes([region[20], region[21]]),
        ],
        bold: region[22] != 0,
        content: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn popover_end_round_trips() {
        // The popover head and its streamed content round-trip at the terminal
        // layer (the content streams between the open and popover_end markers, so
        // a single-frame decode cannot recover it); see the tty_term popover
        // tests. Here we cover the close marker.
        assert_eq!(decode(&encode_popover_end()), Some(Command::PopoverEnd));
    }

    #[test]
    fn rejects_wrong_length_popover_payload() {
        // The first arg here decodes to 3 bytes, not the 23 a popover region
        // needs, and the content arg is absent.
        assert!(decode(b"Gstoatty;popover;YWJj").is_none());
    }

    #[test]
    fn popover_head_round_trips_bold() {
        for bold in [true, false] {
            let command = PopoverCommand {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                fill: [10, 20, 30],
                border: [40, 50, 60],
                content_fg: [70, 80, 90],
                scale: 2,
                offset: [4, -2],
                bold,
                content: String::new(),
            };
            // encode_popover emits the open marker, streamed content, then the
            // close marker. Content is empty here, so slicing off the close
            // marker leaves exactly the head frame to decode.
            let full = encode_popover(&command);
            let head = &full[..full.len() - encode_popover_end().len()];
            assert_eq!(decode(head), Some(Command::Popover(command)));
        }
    }
}
