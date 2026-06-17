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
/// The enum is intentionally exhaustive: adding a variant forces every matcher,
/// including the terminal's apply seam, to handle it rather than silently
/// dropping the new command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Border(BorderCommand),
    Scale(ScaleCommand),
    Popover(PopoverCommand),
    ScrollRegion(ScrollRegionCommand),
    Icon(IconCommand),
    TextRun(TextRunCommand),
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

/// Draw a floating popover region above the grid.
///
/// A `width` by `height` cell box anchored at (`top`, `left`) in absolute grid
/// coordinates, filled with `fill` and outlined with `border`. The region floats
/// above the cells with its own z-order.
///
/// `content` is a line of text drawn inside the box in `content_fg`, drawn at
/// `scale` times the cell size from the box's top-left, clipped to the box.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PopoverCommand {
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
    pub content: String,
}

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
}

/// Which status icon [`IconCommand`] draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
    Error,
    Warning,
    Info,
}

/// Draw a run of text at a fractional scale, vertically centered on a cell row.
///
/// A non-cell component primitive: the run is drawn off the cell grid, so it can
/// be smaller than the grid (a gutter line number) yet still line up with
/// full-size rows. `col` and `row` are the anchor in **sixteenths of a cell**
/// (16 = one cell), so the run can sit at a fractional position; `scale` is the
/// glyph size in **256ths of the cell size** (256 = grid size), so it can be
/// fractional. The run advances one scaled cell width per character and is
/// vertically centered within the target row.
///
/// `bg` is the background the run composites over: the renderer paints each
/// glyph's box opaquely, so for the run to blend cleanly `bg` must match the
/// color already beneath it (a gutter passes the editor's background).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextRunCommand {
    pub col: i16,
    pub row: i16,
    pub scale: u16,
    pub color: [u8; 3],
    pub bg: [u8; 3],
    pub text: String,
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

/// Encode a [`ScaleCommand`] as a full `Gstoatty;scale` frame for an emitter.
pub fn encode_scale(command: &ScaleCommand) -> Vec<u8> {
    let mut arg = Vec::with_capacity(5);
    arg.extend_from_slice(&command.top.to_be_bytes());
    arg.extend_from_slice(&command.left.to_be_bytes());
    arg.push(command.scale);

    frame::encode(&Frame {
        sub: "scale".to_owned(),
        args: vec![arg],
    })
}

/// Encode a [`PopoverCommand`] as a full `Gstoatty;popover` frame for an emitter.
///
/// The region, colors, and scale ride in a fixed 18-byte first argument; the
/// variable content text is a second argument.
pub fn encode_popover(command: &PopoverCommand) -> Vec<u8> {
    let mut region = Vec::with_capacity(22);
    region.extend_from_slice(&command.top.to_be_bytes());
    region.extend_from_slice(&command.left.to_be_bytes());
    region.extend_from_slice(&command.width.to_be_bytes());
    region.extend_from_slice(&command.height.to_be_bytes());
    region.extend_from_slice(&command.fill);
    region.extend_from_slice(&command.border);
    region.extend_from_slice(&command.content_fg);
    region.push(command.scale);
    region.extend_from_slice(&command.offset[0].to_be_bytes());
    region.extend_from_slice(&command.offset[1].to_be_bytes());

    frame::encode(&Frame {
        sub: "popover".to_owned(),
        args: vec![region, command.content.as_bytes().to_vec()],
    })
}

/// Encode a [`ScrollRegionCommand`] as a full `Gstoatty;scroll_region` frame for
/// an emitter.
pub fn encode_scroll_region(command: &ScrollRegionCommand) -> Vec<u8> {
    let mut arg = Vec::with_capacity(10);
    arg.extend_from_slice(&command.top.to_be_bytes());
    arg.extend_from_slice(&command.left.to_be_bytes());
    arg.extend_from_slice(&command.width.to_be_bytes());
    arg.extend_from_slice(&command.height.to_be_bytes());
    arg.extend_from_slice(&command.offset.to_be_bytes());

    frame::encode(&Frame {
        sub: "scroll_region".to_owned(),
        args: vec![arg],
    })
}

/// Encode an [`IconCommand`] as a full `Gstoatty;icon` frame for an emitter.
pub fn encode_icon(command: &IconCommand) -> Vec<u8> {
    let mut arg = Vec::with_capacity(9);
    arg.extend_from_slice(&command.top.to_be_bytes());
    arg.extend_from_slice(&command.left.to_be_bytes());
    arg.push(icon_kind_code(command.kind));
    arg.extend_from_slice(&command.color);
    arg.push(command.size);

    frame::encode(&Frame {
        sub: "icon".to_owned(),
        args: vec![arg],
    })
}

/// Encode a [`TextRunCommand`] as a full `Gstoatty;text_run` frame for an
/// emitter.
///
/// The position, scale, color, and background ride in a fixed 12-byte first
/// argument; the variable run text is a second argument.
pub fn encode_text_run(command: &TextRunCommand) -> Vec<u8> {
    let mut head = Vec::with_capacity(12);
    head.extend_from_slice(&command.col.to_be_bytes());
    head.extend_from_slice(&command.row.to_be_bytes());
    head.extend_from_slice(&command.scale.to_be_bytes());
    head.extend_from_slice(&command.color);
    head.extend_from_slice(&command.bg);

    frame::encode(&Frame {
        sub: "text_run".to_owned(),
        args: vec![head, command.text.as_bytes().to_vec()],
    })
}

/// Map a parsed [`Frame`] to its [`Command`] by sub-command name.
///
/// An unknown sub-command, or a known one whose payload does not parse, yields
/// `None` so the frame is ignored.
fn dispatch(frame: &Frame) -> Option<Command> {
    match frame.sub.as_str() {
        "border" => decode_border(&frame.args).map(Command::Border),
        "scale" => decode_scale(&frame.args).map(Command::Scale),
        "popover" => decode_popover(&frame.args).map(Command::Popover),
        "scroll_region" => decode_scroll_region(&frame.args).map(Command::ScrollRegion),
        "icon" => decode_icon(&frame.args).map(Command::Icon),
        "text_run" => decode_text_run(&frame.args).map(Command::TextRun),
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

fn decode_scale(args: &[Vec<u8>]) -> Option<ScaleCommand> {
    let arg: &[u8; 5] = args.first()?.as_slice().try_into().ok()?;

    Some(ScaleCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        scale: arg[4],
    })
}

fn decode_popover(args: &[Vec<u8>]) -> Option<PopoverCommand> {
    let region: &[u8; 22] = args.first()?.as_slice().try_into().ok()?;
    let content = std::str::from_utf8(args.get(1)?).ok()?.to_owned();

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
        content,
    })
}

fn decode_scroll_region(args: &[Vec<u8>]) -> Option<ScrollRegionCommand> {
    let arg: &[u8; 10] = args.first()?.as_slice().try_into().ok()?;

    Some(ScrollRegionCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        offset: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

fn decode_icon(args: &[Vec<u8>]) -> Option<IconCommand> {
    let arg: &[u8; 9] = args.first()?.as_slice().try_into().ok()?;

    Some(IconCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        kind: decode_icon_kind(arg[4])?,
        color: [arg[5], arg[6], arg[7]],
        size: arg[8],
    })
}

fn decode_text_run(args: &[Vec<u8>]) -> Option<TextRunCommand> {
    let head: &[u8; 12] = args.first()?.as_slice().try_into().ok()?;
    let text = std::str::from_utf8(args.get(1)?).ok()?.to_owned();

    Some(TextRunCommand {
        col: i16::from_be_bytes([head[0], head[1]]),
        row: i16::from_be_bytes([head[2], head[3]]),
        scale: u16::from_be_bytes([head[4], head[5]]),
        color: [head[6], head[7], head[8]],
        bg: [head[9], head[10], head[11]],
        text,
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

fn decode_icon_kind(code: u8) -> Option<IconKind> {
    match code {
        0 => Some(IconKind::Error),
        1 => Some(IconKind::Warning),
        2 => Some(IconKind::Info),
        _ => None,
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
    use super::{
        decode, encode_border, encode_icon, encode_popover, encode_scale, encode_scroll_region,
        encode_text_run, BorderCommand, BorderStyle, Command, IconCommand, IconKind,
        PopoverCommand, ScaleCommand, ScrollRegionCommand, TextRunCommand,
    };

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

    #[test]
    fn popover_round_trips() {
        let command = PopoverCommand {
            top: 3,
            left: 12,
            width: 16,
            height: 4,
            fill: [30, 30, 60],
            border: [200, 200, 255],
            content_fg: [255, 255, 255],
            scale: 2,
            offset: [-3, 5],
            content: "items".to_owned(),
        };

        assert_eq!(
            decode(&encode_popover(&command)),
            Some(Command::Popover(command))
        );
    }

    #[test]
    fn rejects_wrong_length_popover_payload() {
        // The first arg here decodes to 3 bytes, not the 22 a popover region
        // needs, and the content arg is absent.
        assert!(decode(b"Gstoatty;popover;YWJj").is_none());
    }

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

    #[test]
    fn icon_round_trips() {
        let command = IconCommand {
            top: 4,
            left: 1,
            kind: IconKind::Warning,
            color: [255, 200, 0],
            size: 2,
        };

        assert_eq!(decode(&encode_icon(&command)), Some(Command::Icon(command)));
    }

    #[test]
    fn rejects_wrong_length_icon_payload() {
        // The single arg here decodes to 3 bytes, not the 9 an icon needs.
        assert!(decode(b"Gstoatty;icon;YWJj").is_none());
    }

    #[test]
    fn text_run_round_trips() {
        let command = TextRunCommand {
            col: -6,
            row: 80,
            scale: 192,
            color: [180, 190, 200],
            bg: [24, 26, 32],
            text: "127".to_owned(),
        };

        assert_eq!(
            decode(&encode_text_run(&command)),
            Some(Command::TextRun(command))
        );
    }

    #[test]
    fn rejects_wrong_length_text_run_payload() {
        // The first arg here decodes to 3 bytes, not the 9 a text run needs.
        assert!(decode(b"Gstoatty;text_run;YWJj").is_none());
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
