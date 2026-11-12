//! The panel command draws floating chrome around a cell rectangle.
//!
//! Unlike a border, the frame floats under the grid text, so the cells it
//! surrounds keep drawing their own content.

use super::{
    border::{decode_style, style_code, BorderStyle},
    decode, Command,
};
use crate::frame;
use memchr::memmem;
use std::array;

/// The bit of a panel's flags byte that floats it above every pooled surface.
const ABOVE_POOLS: u8 = 1;

/// The bit of a panel's flags byte that makes it a backdrop, which nothing
/// takes as an occluder.
const BACKDROP: u8 = 2;

/// The bit of [`PanelCommand::corners`] that rounds the top-left corner.
pub const CORNER_TOP_LEFT: u8 = 1;
/// The bit of [`PanelCommand::corners`] that rounds the top-right corner.
pub const CORNER_TOP_RIGHT: u8 = 2;
/// The bit of [`PanelCommand::corners`] that rounds the bottom-right corner.
pub const CORNER_BOTTOM_RIGHT: u8 = 4;
/// The bit of [`PanelCommand::corners`] that rounds the bottom-left corner.
pub const CORNER_BOTTOM_LEFT: u8 = 8;

/// The bit of [`PanelCommand::fillets`] that joins the bottom-left corner to the
/// surface below the panel.
pub const FILLET_BOTTOM_LEFT: u8 = 1;
/// The bit of [`PanelCommand::fillets`] that joins the bottom-right corner to the
/// surface below the panel.
pub const FILLET_BOTTOM_RIGHT: u8 = 2;

/// The length of a panel frame's fixed head, which the optional anchor follows.
const HEAD_LEN: usize = 31;

/// Where one side of a panel's box stands against the panel's cells.
///
/// A design states its spacing in pixels, but only the terminal knows how many
/// pixels a cell holds. So an emitter states each edge in logical pixels with a
/// mode, and the terminal resolves the edge against the cell beside it. This
/// lets a box reach across a chrome column or a gap cell and stand a fixed
/// distance from its neighbor at any font size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// The box edge stands this many logical pixels inside the cell edge.
    Inset(u8),
    /// The box crosses the adjacent cell and stops this many logical pixels
    /// short of that cell's far side.
    Margin(u8),
    /// The box crosses into the adjacent cell by half of what [`Edge::Margin`]
    /// reaches.
    ///
    /// A top or left edge rounds its half down, and a bottom or right edge
    /// rounds its half up. So two neighbors that both state `Gap(px)` leave `px`
    /// between them in a cell of any size. A 9 pixel column splits as 3 and 2,
    /// and an 18 pixel row splits as 7 and 7.
    Gap(u8),
    /// The box edge stands this many logical pixels outside the cell edge, at
    /// any cell size.
    Outset(u8),
}

impl Edge {
    /// The edges of a box that stands `x` logical pixels inside its cells on
    /// each side and `y` inside them at the top and the bottom, in
    /// [`PanelCommand::edges`] order.
    pub fn symmetric(x: u8, y: u8) -> [Edge; 4] {
        [
            Edge::Inset(y),
            Edge::Inset(x),
            Edge::Inset(y),
            Edge::Inset(x),
        ]
    }

    /// The logical pixels the edge states, whatever its mode.
    pub fn px(self) -> u8 {
        match self {
            Edge::Inset(px) | Edge::Margin(px) | Edge::Gap(px) | Edge::Outset(px) => px,
        }
    }

    fn mode(self) -> u8 {
        match self {
            Edge::Inset(_) => 0,
            Edge::Margin(_) => 1,
            Edge::Gap(_) => 2,
            Edge::Outset(_) => 3,
        }
    }
}

/// The modes of `edges` packed in one byte, two bits per edge from the low pair
/// in [`PanelCommand::edges`] order: 0 inset, 1 margin, 2 gap, and 3 outset.
///
/// The wire carries this byte, and a renderer that resolves the edges on the
/// GPU unpacks the same word.
pub fn edge_modes(edges: [Edge; 4]) -> u8 {
    edges
        .iter()
        .enumerate()
        .fold(0, |modes, (side, edge)| modes | edge.mode() << (2 * side))
}

/// How a panel's shadow is drawn.
///
/// [`PanelShadow::None_`] draws no shadow. [`PanelShadow::Drop`] is a displaced,
/// blurred shadow that reads as the panel floating above the grid.
/// [`PanelShadow::Tucked`] draws nothing outside the panel, only a dark band
/// inside it that rises from its bottom edge, so the panel reads as a raised
/// card. [`PanelShadow::Overhang`] draws a fainter, shorter band in the same
/// place, so the panel reads as tucked under whatever overhangs it above.
/// [`PanelShadow::Halo`] is undisplaced and soft, clipped at the panel's bottom
/// and right edges, so a panel docked in a bottom-right corner reads as emerging
/// from beneath what sits below and right of it. [`PanelShadow::Cast`] falls a
/// short way below the panel's bottom edge and only between its sides, as a
/// pinned row casts onto the rows that scroll under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelShadow {
    None_,
    Drop,
    Tucked,
    Overhang,
    Halo,
    Cast,
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
///
/// [`Self::edges`] place the box inside its cells or across the cells beside
/// them, and [`Self::corners`] picks the corners the radius rounds.
/// [`Self::fillets`] join the bottom corners to the surface below, and
/// [`Self::hatch`] stripes the fill. Together they draw a chip shorter than its
/// row, a tab rounded at the top alone, a pane's island that reaches into the
/// gap beside it, and a hatched row.
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
    /// Where each side of the box stands against the cells, in the order top,
    /// right, bottom, and left.
    ///
    /// `Edge::symmetric(0, 0)` is cell-exact. An inset draws the box inside its
    /// cells, as a chip shorter than its row does, and a margin or a gap reaches
    /// across the cell beside them. An outset stands the box a fixed distance
    /// outside its cells, as a box taller than its row does. The border, fill,
    /// corner rounding, and shadow all follow the resolved box. A strip between
    /// the box and the cell edge shows what lies behind it.
    pub edges: [Edge; 4],
    /// The corners [`Self::corner_radius`] rounds, one bit each, from
    /// [`CORNER_TOP_LEFT`] to [`CORNER_BOTTOM_LEFT`]. Zero rounds all four, so a
    /// panel that names no corner keeps every corner round. A tab sets the top
    /// two alone and meets the surface below it square.
    pub corners: u8,
    /// The bottom corners joined to the surface below by a fillet,
    /// [`FILLET_BOTTOM_LEFT`] and [`FILLET_BOTTOM_RIGHT`].
    ///
    /// A fillet is a concave curve in the fill color just outside the corner,
    /// so a tab or a docked box flows into the surface it stands on. A panel
    /// with no fill draws none.
    pub fillets: u8,
    /// The radius of each fillet in logical pixels.
    pub fillet_radius: u8,
    /// The color of a hatch over the fill, or `None` for a plain fill.
    ///
    /// The hatch is a 1 logical pixel line every 7 at 135 degrees, placed by the
    /// screen pixel, so the stripes run on unbroken across neighboring panels.
    /// A panel with no fill draws none.
    pub hatch: Option<[u8; 3]>,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it.
    pub above_pools: bool,
    /// The panel sits under everything it overlaps, so no pool composite and no
    /// decoration takes its rect as an occluder.
    ///
    /// This is a pane's island halo, whose box is the pane's own pool region. A
    /// pane pool takes every other panel as an occluder, and the halo must stay
    /// out of that list to keep its pane's glide visible. A terminal before
    /// [`BACKDROP_PANEL_PROTOCOL`](crate::BACKDROP_PANEL_PROTOCOL) reads the
    /// flag as `above_pools`.
    ///
    /// A backdrop paints its fill only outside its own cells, where its edges
    /// reach across the cells beside them. Inside, the cells keep every tint
    /// they draw, because a fill over them hides those tints.
    ///
    /// A backdrop with a fill also paints its `border` color over the part of
    /// its own cells that lies outside its rounded box, above the text and the
    /// pool composites. A rounded corner inside the cells then shows the
    /// surface the panel stands on. A backdrop with no fill paints none.
    pub backdrop: bool,
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
        w.write_all(&[edge_modes(command.edges)])?;
        w.write_all(&[flags(command)])?;
        w.write_all(&command.edges.map(Edge::px))?;
        w.write_all(&[command.corners, command.fillets, command.fillet_radius])?;
        w.write_all(&[u8::from(command.hatch.is_some())])?;
        w.write_all(&command.hatch.unwrap_or([0, 0, 0]))?;
        // The anchor trails the fixed head, and a screen-fixed panel leaves it
        // out.
        if let Some((host, top_rows)) = command.anchor {
            w.write_all(&host.to_be_bytes())?;
            w.write_all(&top_rows.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);
}

/// The bytes that open every frame [`encode_panel_into`] writes.
const PANEL_HEAD: &[u8] = b"\x1b_Gstoatty;panel;";

/// Every panel that a run of emitted frames declares, in emission order.
///
/// An emitter reads its own scene back with this when it needs the rects its
/// panels cover. Only the panel frames decode, so a scene of text runs and bars
/// costs one search of its bytes.
pub fn decode_panels(bytes: &[u8]) -> impl Iterator<Item = PanelCommand> {
    memmem::find_iter(bytes, PANEL_HEAD).filter_map(|at| {
        let frame = &bytes[at..];
        let end = frame::first_frame_end(frame)?;
        match decode(&frame[..end])? {
            Command::Panel(panel) => Some(panel),
            _ => None,
        }
    })
}

pub(super) fn decode_panel(args: &[Vec<u8>]) -> Option<PanelCommand> {
    let arg = args.first()?;
    if arg.len() < HEAD_LEN {
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
        edges: decode_edges(arg[18], [arg[20], arg[21], arg[22], arg[23]]),
        // A bit this decoder does not know reads as above pools, the layer every
        // nonzero byte meant before the flags had more than one bit.
        above_pools: arg[19] & !BACKDROP != 0,
        backdrop: arg[19] & BACKDROP != 0,
        corners: arg[24],
        fillets: arg[25],
        fillet_radius: arg[26],
        hatch: (arg[27] != 0).then_some([arg[28], arg[29], arg[30]]),
        anchor: arg.get(HEAD_LEN..HEAD_LEN + 8).map(|tail| {
            (
                u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]),
                f32::from_be_bytes([tail[4], tail[5], tail[6], tail[7]]),
            )
        }),
    })
}

/// The flags byte that carries `command`'s layering bits.
fn flags(command: &PanelCommand) -> u8 {
    let above = if command.above_pools { ABOVE_POOLS } else { 0 };
    let backdrop = if command.backdrop { BACKDROP } else { 0 };
    above | backdrop
}

fn shadow_code(shadow: PanelShadow) -> u8 {
    match shadow {
        PanelShadow::None_ => 0,
        PanelShadow::Drop => 1,
        PanelShadow::Tucked => 2,
        PanelShadow::Overhang => 3,
        PanelShadow::Halo => 4,
        PanelShadow::Cast => 5,
    }
}

/// An unknown code falls back to [`PanelShadow::Drop`], the visible default, so a
/// newer emitter's added style still shows a shadow on an older reader.
fn decode_shadow(code: u8) -> PanelShadow {
    match code {
        0 => PanelShadow::None_,
        2 => PanelShadow::Tucked,
        3 => PanelShadow::Overhang,
        4 => PanelShadow::Halo,
        5 => PanelShadow::Cast,
        _ => PanelShadow::Drop,
    }
}

/// The edges that an [`edge_modes`] byte and four px bytes state.
fn decode_edges(modes: u8, px: [u8; 4]) -> [Edge; 4] {
    array::from_fn(|side| match (modes >> (2 * side)) & 0b11 {
        1 => Edge::Margin(px[side]),
        2 => Edge::Gap(px[side]),
        3 => Edge::Outset(px[side]),
        _ => Edge::Inset(px[side]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{encode_pool_drop_into, encode_reset_into};

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
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
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
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: true,
            backdrop: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    /// A read-only pane's ring is a dashed frame with no fill and no shadow.
    #[test]
    fn dashed_panel_round_trips() {
        let command = PanelCommand {
            top: 1,
            left: 6,
            width: 70,
            height: 20,
            style: BorderStyle::Dashed,
            border: [138, 118, 80],
            corner_radius: 2,
            fill: None,
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    /// A backdrop panel keeps its flag apart from the above-pools one through
    /// the shared flags byte.
    #[test]
    fn panel_backdrop_round_trips() {
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 10,
            style: BorderStyle::None_,
            border: [18, 20, 24],
            corner_radius: 2,
            fill: None,
            shadow: PanelShadow::Tucked,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: true,
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
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
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
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
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
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_halo_shadow_round_trips() {
        let command = PanelCommand {
            top: 1,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::None_,
            border: [1, 2, 3],
            corner_radius: 2,
            fill: Some([4, 5, 6]),
            shadow: PanelShadow::Halo,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: true,
            backdrop: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn panel_cast_shadow_round_trips() {
        let command = PanelCommand {
            top: 1,
            left: 0,
            width: 12,
            height: 2,
            style: BorderStyle::None_,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::Cast,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    #[test]
    fn unknown_shadow_code_falls_back_to_drop() {
        assert_eq!(
            [decode_shadow(3), decode_shadow(4), decode_shadow(5)],
            [PanelShadow::Overhang, PanelShadow::Halo, PanelShadow::Cast]
        );
        assert_eq!(
            decode_shadow(6),
            PanelShadow::Drop,
            "unknown code is a drop shadow"
        );
    }

    #[test]
    fn decode_panels_reads_each_panel_among_other_frames() {
        let panel = |top, anchor| PanelCommand {
            top,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::Light,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor,
        };
        let (first, second) = (panel(1, None), panel(9, Some((3, 1.5))));
        let mut stream = Vec::new();
        encode_reset_into(&mut stream);
        encode_panel_into(&mut stream, &first);
        encode_pool_drop_into(&mut stream, 7);
        encode_panel_into(&mut stream, &second);

        assert_eq!(decode_panels(&stream).collect::<Vec<_>>(), [first, second]);
    }

    #[test]
    fn rejects_wrong_length_panel_payload() {
        // The single arg here decodes to 3 bytes, not the 31 a panel head needs.
        assert!(decode(b"Gstoatty;panel;YWJj").is_none());
    }

    /// The shape bytes carry a vertical inset, a corner mask, a fillet mask, and
    /// a hatch, and the anchor follows them, so a tab that rides a pane keeps
    /// every part of its shape.
    #[test]
    fn panel_shape_fields_round_trip_before_the_anchor() {
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 1,
            style: BorderStyle::None_,
            border: [1, 2, 3],
            corner_radius: 2,
            fill: Some([20, 22, 30]),
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 1),
            corners: CORNER_TOP_LEFT | CORNER_TOP_RIGHT,
            fillets: FILLET_BOTTOM_LEFT | FILLET_BOTTOM_RIGHT,
            fillet_radius: 2,
            hatch: Some([7, 8, 9]),
            above_pools: false,
            backdrop: false,
            anchor: Some((7, 12.5)),
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    /// Each edge keeps its mode and its pixels, and the fillet radius keeps its
    /// own byte. The mode byte packs two bits per edge from the low pair, top
    /// first, which is the word a renderer unpacks on the GPU.
    #[test]
    fn panel_edges_and_fillet_radius_round_trip() {
        let edges = [
            Edge::Gap(4),
            Edge::Margin(4),
            Edge::Outset(2),
            Edge::Gap(250),
        ];
        let command = PanelCommand {
            top: 3,
            left: 12,
            width: 40,
            height: 10,
            style: BorderStyle::None_,
            border: [1, 2, 3],
            corner_radius: 2,
            fill: Some([20, 22, 30]),
            shadow: PanelShadow::Tucked,
            edges,
            corners: CORNER_TOP_LEFT,
            fillets: FILLET_BOTTOM_LEFT,
            fillet_radius: 3,
            hatch: None,
            above_pools: false,
            backdrop: true,
            anchor: Some((7, 12.5)),
        };

        assert_eq!(
            edge_modes(edges),
            0b1011_0110,
            "a top gap in bits 0-1, a right margin in bits 2-3, a bottom outset in bits 4-5, \
             a left gap in bits 6-7",
        );
        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
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
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: true,
            backdrop: false,
            anchor: Some((7, 12.5)),
        };

        assert_eq!(
            decode(&encode_panel(&command)),
            Some(Command::Panel(command))
        );
    }

    /// A head shorter than the fixed 31 bytes decodes to nothing, whatever an
    /// older emitter wrote into the bytes it has.
    #[test]
    fn a_short_panel_head_decodes_to_nothing() {
        let head = |len: usize| decode_panel(&[vec![1u8; len]]);

        assert_eq!([19, 20, 22, 26, 27, 30].map(head), [None; 6]);
        assert!(head(31).is_some(), "the full head decodes");
    }
}
