//! The panel command draws floating chrome around a cell rectangle.
//!
//! Unlike a border, the frame floats under the grid text, so the cells it
//! surrounds keep drawing their own content.

use super::border::{decode_style, style_code, BorderStyle};
use crate::frame;

/// How a panel's shadow is drawn.
///
/// [`PanelShadow::None_`] draws no shadow. [`PanelShadow::Drop`] is a displaced,
/// blurred shadow that reads as the panel floating above the grid.
/// [`PanelShadow::Tucked`] is undisplaced with a tight halo clipped above the
/// panel's bottom edge, so the panel reads as emerging from beneath whatever sits
/// below it rather than floating in front. [`PanelShadow::Overhang`] draws no
/// exterior halo at all, only a small shadow band inside the panel along its
/// bottom edge, so the panel reads as tucked under whatever overhangs it above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelShadow {
    None_,
    Drop,
    Tucked,
    Overhang,
}

/// Draw off-grid modal chrome framing a cell rectangle.
///
/// A `width` by `height` cell region at (`top`, `left`) in absolute grid
/// coordinates gets a hairline frame in `border` at `style` weight, with
/// `corner_radius` logical-pixel rounded corners (0 = square) and a `shadow`
/// drawn in the selected [`PanelShadow`] style. Unlike a per-cell
/// [`BorderCommand`], the frame is a floating component drawn under the grid
/// text, so the framed cells keep rendering their own content.
///
/// `fill` is [`Some`] to paint the interior that color, or [`None`] to leave the
/// cells' own SGR backgrounds showing through.
// `Eq` is absent because [`Self::anchor`] carries a fractional row offset and
// `f32` is only `PartialEq`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PanelCommand {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub style: BorderStyle,
    pub border: [u8; 3],
    pub corner_radius: u8,
    pub fill: Option<[u8; 3]>,
    pub shadow: PanelShadow,
    /// Logical pixels shaved off each horizontal edge, so the box draws narrower
    /// than its cell rect. `0` is cell-exact. The border, fill, corner rounding,
    /// and shadow all follow the inset rect, leaving the strip outside it showing
    /// the cells behind.
    pub inset_x: u8,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it.
    pub above_pools: bool,
    /// The host pool this panel rides and the document top row its layout
    /// assumed, or `None` for a panel fixed to the screen.
    ///
    /// `Some((host, top_rows))` asks the terminal to draw the frame shifted by
    /// the gap between `top_rows` and the host's eased top, so a popup framed
    /// over a scrolling pane travels with the text beneath it. The panel's
    /// counterpart to [`PoolAnchorCommand`](super::PoolAnchorCommand), which
    /// carries the same tie for the popup's own pool region.
    pub anchor: Option<(u32, f32)>,
}

/// Encode a [`PanelCommand`] as a full `Gstoatty;panel` frame for an emitter.
pub fn encode_panel(command: &PanelCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_panel_into(&mut out, command);
    out
}

/// Append a `Gstoatty;panel` frame for `command` to `out` without allocating.
pub fn encode_panel_into(out: &mut Vec<u8>, command: &PanelCommand) {
    frame::begin(out, "panel");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&[style_code(command.style)])?;
        w.write_all(&command.border)?;
        w.write_all(&[command.corner_radius])?;
        w.write_all(&[command.fill.is_some() as u8])?;
        w.write_all(&command.fill.unwrap_or([0, 0, 0]))?;
        w.write_all(&[shadow_code(command.shadow)])?;
        w.write_all(&[command.inset_x])?;
        w.write_all(&[command.above_pools as u8])?;
        // Trailing and optional, so a receiver built before the anchor existed
        // reads the frame whole and treats the panel as screen-fixed.
        if let Some((host, top_rows)) = command.anchor {
            w.write_all(&host.to_be_bytes())?;
            w.write_all(&top_rows.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);
}

pub(super) fn decode_panel(args: &[Vec<u8>]) -> Option<PanelCommand> {
    let arg = args.first()?;
    if arg.len() < 19 {
        return None;
    }

    Some(PanelCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        style: decode_style(arg[8]),
        border: [arg[9], arg[10], arg[11]],
        corner_radius: arg[12],
        fill: (arg[13] != 0).then_some([arg[14], arg[15], arg[16]]),
        shadow: decode_shadow(arg[17]),
        inset_x: arg[18],
        above_pools: arg.get(19).is_some_and(|byte| *byte != 0),
        anchor: arg.get(20..28).map(|tail| {
            (
                u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]),
                f32::from_be_bytes([tail[4], tail[5], tail[6], tail[7]]),
            )
        }),
    })
}

fn shadow_code(shadow: PanelShadow) -> u8 {
    match shadow {
        PanelShadow::None_ => 0,
        PanelShadow::Drop => 1,
        PanelShadow::Tucked => 2,
        PanelShadow::Overhang => 3,
    }
}

/// An unknown code falls back to [`PanelShadow::Drop`], the visible default, so a
/// newer emitter's added style still shows a shadow on an older reader.
fn decode_shadow(code: u8) -> PanelShadow {
    match code {
        0 => PanelShadow::None_,
        2 => PanelShadow::Tucked,
        3 => PanelShadow::Overhang,
        _ => PanelShadow::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn panel_round_trips() {
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 10,
            style: BorderStyle::Rounded,
            border: [200, 40, 90],
            corner_radius: 6,
            fill: Some([20, 22, 30]),
            shadow: PanelShadow::Drop,
            inset_x: 4,
            above_pools: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_above_pools_round_trips() {
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 10,
            style: BorderStyle::Rounded,
            border: [200, 40, 90],
            corner_radius: 6,
            fill: Some([20, 22, 30]),
            shadow: PanelShadow::Drop,
            inset_x: 4,
            above_pools: true,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_without_fill_round_trips() {
        let command = PanelCommand {
            top: 0,
            left: 0,
            width: 8,
            height: 4,
            style: BorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_tucked_shadow_round_trips() {
        let command = PanelCommand {
            top: 1,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: Some([4, 5, 6]),
            shadow: PanelShadow::Tucked,
            inset_x: 4,
            above_pools: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_overhang_shadow_round_trips() {
        let command = PanelCommand {
            top: 1,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: Some([4, 5, 6]),
            shadow: PanelShadow::Overhang,
            inset_x: 4,
            above_pools: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn unknown_shadow_code_falls_back_to_drop() {
        assert_eq!(decode_shadow(3), PanelShadow::Overhang);
        assert_eq!(
            decode_shadow(4),
            PanelShadow::Drop,
            "unknown code is a drop shadow"
        );
    }

    #[test]
    fn rejects_wrong_length_panel_payload() {
        // The single arg here decodes to 3 bytes, not the 18 a panel needs.
        assert!(decode(b"Gstoatty;panel;YWJj").is_none());
    }

    #[test]
    fn panel_decode_tolerates_legacy_title_gap_bytes() {
        // A 22-byte arg carries three trailing bytes past the 19-byte base from an
        // emitter that still wrote the retired title-gap span. The decoder reads
        // what it knows rather than rejecting the frame. Byte 19 now holds the
        // above_pools flag, so the retired gap start's nonzero low byte reads as a
        // set flag and only the two bytes past it are ignored.
        let mut arg = Vec::new();
        arg.extend_from_slice(&3u16.to_be_bytes());
        arg.extend_from_slice(&12u16.to_be_bytes());
        arg.extend_from_slice(&40u16.to_be_bytes());
        arg.extend_from_slice(&10u16.to_be_bytes());
        arg.push(style_code(BorderStyle::Rounded));
        arg.extend_from_slice(&[200, 40, 90]);
        arg.push(6);
        arg.push(1);
        arg.extend_from_slice(&[20, 22, 30]);
        arg.push(1);
        arg.extend_from_slice(&16u16.to_be_bytes());
        arg.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(arg.len(), 22);

        assert_eq!(
            decode_panel(&[arg]),
            Some(PanelCommand {
                top: 3,
                left: 12,
                width: 40,
                height: 10,
                style: BorderStyle::Rounded,
                border: [200, 40, 90],
                corner_radius: 6,
                fill: Some([20, 22, 30]),
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools: true,
                anchor: None,
            })
        );
    }

    #[test]
    fn panel_anchor_round_trips() {
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 10,
            style: BorderStyle::Rounded,
            border: [200, 40, 90],
            corner_radius: 6,
            fill: Some([20, 22, 30]),
            shadow: PanelShadow::Drop,
            inset_x: 4,
            above_pools: true,
            anchor: Some((7, 12.5)),
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_decode_defaults_an_anchorless_frame_to_screen_fixed() {
        // A 20-byte arg carries the above_pools flag but predates the anchor.
        // An emitter left over from before a mid-session rebuild still decodes,
        // its panel holding its screen position as it did when written.
        let mut arg = Vec::new();
        arg.extend_from_slice(&3u16.to_be_bytes());
        arg.extend_from_slice(&12u16.to_be_bytes());
        arg.extend_from_slice(&40u16.to_be_bytes());
        arg.extend_from_slice(&10u16.to_be_bytes());
        arg.push(style_code(BorderStyle::Rounded));
        arg.extend_from_slice(&[200, 40, 90]);
        arg.push(6);
        arg.push(0);
        arg.extend_from_slice(&[0, 0, 0]);
        arg.push(shadow_code(PanelShadow::Drop));
        arg.push(4);
        arg.push(1);
        assert_eq!(arg.len(), 20);

        assert_eq!(
            decode_panel(&[arg]).map(|panel| (panel.above_pools, panel.anchor)),
            Some((true, None)),
        );
    }

    #[test]
    fn panel_decode_defaults_a_flagless_frame_to_grid_layering() {
        // A 19-byte arg predates the above_pools flag. An emitter left over from
        // before a mid-session rebuild still decodes, its panel layered with the
        // grid as it was when the frame was written.
        let mut arg = Vec::new();
        arg.extend_from_slice(&3u16.to_be_bytes());
        arg.extend_from_slice(&12u16.to_be_bytes());
        arg.extend_from_slice(&40u16.to_be_bytes());
        arg.extend_from_slice(&10u16.to_be_bytes());
        arg.push(style_code(BorderStyle::Rounded));
        arg.extend_from_slice(&[200, 40, 90]);
        arg.push(6);
        arg.push(0);
        arg.extend_from_slice(&[0, 0, 0]);
        arg.push(shadow_code(PanelShadow::Tucked));
        arg.push(4);
        assert_eq!(arg.len(), 19);

        assert_eq!(
            decode_panel(&[arg]),
            Some(PanelCommand {
                top: 3,
                left: 12,
                width: 40,
                height: 10,
                style: BorderStyle::Rounded,
                border: [200, 40, 90],
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::Tucked,
                inset_x: 4,
                above_pools: false,
                anchor: None,
            })
        );
    }
}
