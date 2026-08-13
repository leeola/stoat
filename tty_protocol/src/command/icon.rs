//! The icon command composites a renderer-drawn status shape at a grid cell.
//!
//! The shape is a silhouette the terminal draws, not a glyph, so the icon set
//! stays fixed and stays crisp at any size.

use crate::frame;

/// Composite a fixed renderer-drawn status icon at a grid cell.
///
/// The icon is a signed-distance shape, not a glyph or image: the terminal draws
/// the [`IconKind`] silhouette in `color` over a `size` by `size` cell block
/// anchored at (`top`, `left`) in absolute grid coordinates. Carrying the kind
/// rather than a codepoint keeps the icon set fixed and crisp at any size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IconCommand {
    pub top: u16,
    pub left: u16,
    pub kind: IconKind,
    pub color: [u8; 3],
    pub size: u8,
    /// Signed `[x, y]` pixel offset from the anchor cell, mirroring
    /// [`PopoverCommand::offset`], so the icon can shift inside a popover's inset
    /// content rather than snapping to the cell grid. The one-cell sigil fallback
    /// ignores it.
    pub offset: [i16; 2],
}

/// Which status icon [`IconCommand`] draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
    Error,
    Warning,
    Info,
}

/// Encode an [`IconCommand`] as a full `Gstoatty;icon` frame for an emitter.
pub fn encode_icon(command: &IconCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_icon_into(&mut out, command);
    out
}

/// Append a `Gstoatty;icon` frame for `command` to `out` without allocating.
pub fn encode_icon_into(out: &mut Vec<u8>, command: &IconCommand) {
    frame::begin(out, "icon");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&[icon_kind_code(command.kind)])?;
        w.write_all(&command.color)?;
        w.write_all(&[command.size])?;
        w.write_all(&command.offset[0].to_be_bytes())?;
        w.write_all(&command.offset[1].to_be_bytes())
    });
    frame::end(out);
}

pub(super) fn decode_icon(args: &[Vec<u8>]) -> Option<IconCommand> {
    let arg = args.first()?;
    if arg.len() < 9 {
        return None;
    }
    // The offset was added after the initial 9-byte layout, so a 9-byte arg is a
    // legacy frame that predates it and decodes to no offset.
    let offset = if arg.len() >= 13 {
        [
            i16::from_be_bytes([arg[9], arg[10]]),
            i16::from_be_bytes([arg[11], arg[12]]),
        ]
    } else {
        [0, 0]
    };

    Some(IconCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        kind: decode_icon_kind(arg[4]),
        color: [arg[5], arg[6], arg[7]],
        size: arg[8],
        offset,
    })
}

/// An unknown code falls back to `Info` rather than killing the command, so a
/// kind added later still marks its line, at the mildest severity an older
/// terminal knows, instead of leaving nothing there.
pub(super) fn decode_icon_kind(code: u8) -> IconKind {
    match code {
        0 => IconKind::Error,
        1 => IconKind::Warning,
        _ => IconKind::Info,
    }
}

fn icon_kind_code(kind: IconKind) -> u8 {
    match kind {
        IconKind::Error => 0,
        IconKind::Warning => 1,
        IconKind::Info => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn icon_round_trips() {
        let command = IconCommand {
            top: 4,
            left: 1,
            kind: IconKind::Warning,
            color: [255, 200, 0],
            size: 2,
            offset: [-3, 6],
        };

        assert_eq!(decode(&encode_icon(&command)), Some(Command::Icon(command)));
    }

    #[test]
    fn rejects_wrong_length_icon_payload() {
        // The single arg here decodes to 3 bytes, not the 9 an icon needs.
        assert!(decode(b"Gstoatty;icon;YWJj").is_none());
    }

    #[test]
    fn icon_decodes_legacy_arg_without_offset() {
        // A 9-byte arg predates the offset field and decodes to no offset.
        let mut arg = Vec::new();
        arg.extend_from_slice(&4u16.to_be_bytes());
        arg.extend_from_slice(&1u16.to_be_bytes());
        arg.push(icon_kind_code(IconKind::Warning));
        arg.extend_from_slice(&[255, 200, 0]);
        arg.push(2);
        assert_eq!(arg.len(), 9);

        assert_eq!(
            decode_icon(&[arg]),
            Some(IconCommand {
                top: 4,
                left: 1,
                kind: IconKind::Warning,
                color: [255, 200, 0],
                size: 2,
                offset: [0, 0],
            })
        );
    }
}
