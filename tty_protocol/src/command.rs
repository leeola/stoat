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
    Bar(BarCommand),
    LineLayout(LineLayoutCommand),
    /// Open the page-fill redirect onto a recycled pool slot.
    ///
    /// The streamed bytes that follow paint the page named by
    /// [`FillCommand::index`] instead of the live grid, until [`Command::FillEnd`]
    /// (or the next `fill`/`reset`) commits the slot and restores the live grid.
    Fill(FillCommand),
    /// Close the page-fill redirect opened by [`Command::Fill`].
    ///
    /// Commits the page painted since the open marker onto its pool slot and
    /// restores the live grid as the write target. Carries no payload.
    FillEnd,
    /// Set the smooth-scroll target to an app-declared document offset.
    ///
    /// The renderer eases the live scroll offset toward [`ScrollCommand`]'s
    /// page-plus-fraction position over subsequent frames, so the program
    /// reports where it wants the viewport and the terminal owns the animation.
    Scroll(ScrollCommand),
    /// Clear all accumulated stoatty decoration state, so the program can redraw
    /// its scene from scratch. Carries no payload.
    Reset,
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

/// Declare the surface's logical-line layout: the height in rows of each logical
/// line, indexed from the top.
///
/// Most lines are one row; a height greater than one is an integer-cell inline
/// expansion (an inline diff, a multi-line diagnostic) that pushes every later
/// line down. A line past the end of [`Self::heights`] defaults to one row. A
/// non-cell component bound to a logical line reads the prefix sum of these
/// heights to find the physical row it sits on, so it tracks expansions. The
/// full layout is sent on each change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineLayoutCommand {
    pub heights: Vec<u16>,
}

/// Name the document page a [`Command::Fill`] redirect paints into.
///
/// The open half of the `fill`/`fill_end` marker pair. A page is a full grid of
/// cells, far larger than the APC frame cap, so it cannot ride a frame payload:
/// this marker only names the target page, and the page's content streams as
/// ordinary VT + SGR bytes after the frame, committed when the redirect closes.
/// `index` is the app's document page index, the same key the pool slot is
/// addressed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FillCommand {
    pub index: u64,
}

/// A smooth-scroll target as a document-page offset.
///
/// Names where the program wants the viewport: `page` is the document page index
/// (the same key the page pool is addressed by) and `fraction` is the sub-page
/// position within it, in 1/65536ths of a page. The renderer eases the live
/// offset toward this position rather than jumping, so the program reports an
/// absolute target and the terminal animates toward it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollCommand {
    pub page: u64,
    pub fraction: u16,
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

/// Encode a [`PopoverCommand`] as a full `Gstoatty;popover` frame for an emitter.
///
/// The region, colors, and scale ride in a fixed 18-byte first argument; the
/// variable content text is a second argument.
pub fn encode_popover(command: &PopoverCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_popover_into(
        &mut out,
        command.top,
        command.left,
        command.width,
        command.height,
        command.fill,
        command.border,
        command.content_fg,
        command.scale,
        command.offset,
        &command.content,
    );
    out
}

/// Append a `Gstoatty;popover` frame to `out` without allocating.
///
/// `content` is borrowed so an emitter can pass a slice of its own buffer rather
/// than build an owned [`String`] per frame. The fixed region fields ride in the
/// first argument; `content` is the second.
#[allow(clippy::too_many_arguments)]
pub fn encode_popover_into(
    out: &mut Vec<u8>,
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    fill: [u8; 3],
    border: [u8; 3],
    content_fg: [u8; 3],
    scale: u8,
    offset: [i16; 2],
    content: &str,
) {
    frame::begin(out, "popover");
    frame::push_arg(out, |w| {
        w.write_all(&top.to_be_bytes())?;
        w.write_all(&left.to_be_bytes())?;
        w.write_all(&width.to_be_bytes())?;
        w.write_all(&height.to_be_bytes())?;
        w.write_all(&fill)?;
        w.write_all(&border)?;
        w.write_all(&content_fg)?;
        w.write_all(&[scale])?;
        w.write_all(&offset[0].to_be_bytes())?;
        w.write_all(&offset[1].to_be_bytes())
    });
    frame::push_arg(out, |w| w.write_all(content.as_bytes()));
    frame::end(out);
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
        w.write_all(&[command.size])
    });
    frame::end(out);
}

/// Encode a [`TextRunCommand`] as a full `Gstoatty;text_run` frame for an
/// emitter.
///
/// The position, scale, color, and background ride in a fixed 12-byte first
/// argument; the variable run text is a second argument.
pub fn encode_text_run(command: &TextRunCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_text_run_into(
        &mut out,
        command.col,
        command.row,
        command.scale,
        command.color,
        command.bg,
        &command.text,
    );
    out
}

/// Append a `Gstoatty;text_run` frame to `out` without allocating.
///
/// `text` is borrowed so an emitter can pass a slice of a reused buffer (a gutter
/// formats line numbers into a stack buffer) rather than build an owned
/// [`String`] per frame. The fixed head fields ride in the first argument; `text`
/// is the second.
pub fn encode_text_run_into(
    out: &mut Vec<u8>,
    col: i16,
    row: i16,
    scale: u16,
    color: [u8; 3],
    bg: [u8; 3],
    text: &str,
) {
    frame::begin(out, "text_run");
    frame::push_arg(out, |w| {
        w.write_all(&col.to_be_bytes())?;
        w.write_all(&row.to_be_bytes())?;
        w.write_all(&scale.to_be_bytes())?;
        w.write_all(&color)?;
        w.write_all(&bg)
    });
    frame::push_arg(out, |w| w.write_all(text.as_bytes()));
    frame::end(out);
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

/// Encode a [`LineLayoutCommand`] as a full `Gstoatty;line_layout` frame for an
/// emitter.
///
/// The per-line heights ride in a single argument as consecutive big-endian
/// `u16`s.
pub fn encode_line_layout(command: &LineLayoutCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_line_layout_into(&mut out, &command.heights);
    out
}

/// Append a `Gstoatty;line_layout` frame for `heights` to `out` without
/// allocating.
///
/// `heights` is borrowed and streamed as consecutive big-endian `u16`s straight
/// into the base64 sink, so no intermediate byte buffer is built.
pub fn encode_line_layout_into(out: &mut Vec<u8>, heights: &[u16]) {
    frame::begin(out, "line_layout");
    frame::push_arg(out, |w| {
        for height in heights {
            w.write_all(&height.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);
}

/// Encode a [`FillCommand`] as a full `Gstoatty;fill` open-marker frame.
///
/// The page index rides in a single fixed 8-byte big-endian argument; the
/// page's content streams as VT bytes after the frame, not as a frame argument.
pub fn encode_fill(command: &FillCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_fill_into(&mut out, command.index);
    out
}

/// Append a `Gstoatty;fill` open-marker frame for page `index` to `out`.
pub fn encode_fill_into(out: &mut Vec<u8>, index: u64) {
    frame::begin(out, "fill");
    frame::push_arg(out, |w| w.write_all(&index.to_be_bytes()));
    frame::end(out);
}

/// Encode a [`Command::FillEnd`] as a full `Gstoatty;fill_end` close-marker
/// frame.
///
/// The frame carries no arguments; receiving it commits the page painted since
/// the matching [`Command::Fill`] onto its pool slot and restores the live grid.
pub fn encode_fill_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_fill_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;fill_end` close-marker frame to `out`.
pub fn encode_fill_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "fill_end");
    frame::end(out);
}

/// Encode a [`ScrollCommand`] as a full `Gstoatty;scroll` frame for an emitter.
///
/// The page and sub-page fraction ride in a single fixed 10-byte argument.
pub fn encode_scroll(command: &ScrollCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scroll_into(&mut out, command);
    out
}

/// Append a `Gstoatty;scroll` frame for `command` to `out` without allocating.
pub fn encode_scroll_into(out: &mut Vec<u8>, command: &ScrollCommand) {
    frame::begin(out, "scroll");
    frame::push_arg(out, |w| {
        w.write_all(&command.page.to_be_bytes())?;
        w.write_all(&command.fraction.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`Command::Reset`] as a full `Gstoatty;reset` frame for an emitter.
///
/// The frame carries no arguments; receiving it clears all accumulated stoatty
/// decoration state so the program can redraw its scene from scratch.
pub fn encode_reset() -> Vec<u8> {
    let mut out = Vec::new();
    encode_reset_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;reset` frame to `out`.
pub fn encode_reset_into(out: &mut Vec<u8>) {
    frame::begin(out, "reset");
    frame::end(out);
}

/// Append the full `Gstoatty` frame for any [`Command`] to `out` without
/// allocating, dispatching on the variant.
///
/// The encode-side mirror of [`decode`]: an emitter assembling a scene appends
/// each command into one reused buffer.
pub fn encode_into(out: &mut Vec<u8>, command: &Command) {
    match command {
        Command::Border(c) => encode_border_into(out, c),
        Command::Scale(c) => encode_scale_into(out, c),
        Command::Popover(c) => encode_popover_into(
            out,
            c.top,
            c.left,
            c.width,
            c.height,
            c.fill,
            c.border,
            c.content_fg,
            c.scale,
            c.offset,
            &c.content,
        ),
        Command::ScrollRegion(c) => encode_scroll_region_into(out, c),
        Command::Icon(c) => encode_icon_into(out, c),
        Command::TextRun(c) => {
            encode_text_run_into(out, c.col, c.row, c.scale, c.color, c.bg, &c.text)
        },
        Command::Bar(c) => encode_bar_into(out, c),
        Command::LineLayout(c) => encode_line_layout_into(out, &c.heights),
        Command::Fill(c) => encode_fill_into(out, c.index),
        Command::FillEnd => encode_fill_end_into(out),
        Command::Scroll(c) => encode_scroll_into(out, c),
        Command::Reset => encode_reset_into(out),
    }
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
        "bar" => decode_bar(&frame.args).map(Command::Bar),
        "line_layout" => decode_line_layout(&frame.args).map(Command::LineLayout),
        "fill" => decode_fill(&frame.args).map(Command::Fill),
        "fill_end" => Some(Command::FillEnd),
        "scroll" => decode_scroll(&frame.args).map(Command::Scroll),
        "reset" => Some(Command::Reset),
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

fn decode_bar(args: &[Vec<u8>]) -> Option<BarCommand> {
    let arg: &[u8; 11] = args.first()?.as_slice().try_into().ok()?;

    Some(BarCommand {
        x: i16::from_be_bytes([arg[0], arg[1]]),
        y: i16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        color: [arg[8], arg[9], arg[10]],
    })
}

fn decode_line_layout(args: &[Vec<u8>]) -> Option<LineLayoutCommand> {
    let arg = args.first()?;
    if arg.len() % 2 != 0 {
        return None;
    }

    let heights = arg
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    Some(LineLayoutCommand { heights })
}

fn decode_fill(args: &[Vec<u8>]) -> Option<FillCommand> {
    let arg: &[u8; 8] = args.first()?.as_slice().try_into().ok()?;

    Some(FillCommand {
        index: u64::from_be_bytes(*arg),
    })
}

fn decode_scroll(args: &[Vec<u8>]) -> Option<ScrollCommand> {
    let arg: &[u8; 10] = args.first()?.as_slice().try_into().ok()?;

    Some(ScrollCommand {
        page: u64::from_be_bytes([
            arg[0], arg[1], arg[2], arg[3], arg[4], arg[5], arg[6], arg[7],
        ]),
        fraction: u16::from_be_bytes([arg[8], arg[9]]),
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
        decode, encode_bar, encode_border, encode_fill, encode_fill_end, encode_icon, encode_into,
        encode_line_layout, encode_popover, encode_reset, encode_scale, encode_scroll,
        encode_scroll_region, encode_text_run, BarCommand, BorderCommand, BorderStyle, Command,
        FillCommand, IconCommand, IconKind, LineLayoutCommand, PopoverCommand, ScaleCommand,
        ScrollCommand, ScrollRegionCommand, TextRunCommand,
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

    #[test]
    fn line_layout_round_trips() {
        let command = LineLayoutCommand {
            heights: vec![1, 3, 1, 2],
        };

        assert_eq!(
            decode(&encode_line_layout(&command)),
            Some(Command::LineLayout(command))
        );
    }

    #[test]
    fn rejects_odd_length_line_layout_payload() {
        // The single arg here decodes to 3 bytes, not a whole number of u16s.
        assert!(decode(b"Gstoatty;line_layout;YWJj").is_none());
    }

    #[test]
    fn fill_round_trips() {
        let command = FillCommand {
            index: 4_000_000_000,
        };

        assert_eq!(decode(&encode_fill(&command)), Some(Command::Fill(command)));
    }

    #[test]
    fn fill_end_round_trips() {
        assert_eq!(decode(&encode_fill_end()), Some(Command::FillEnd));
    }

    #[test]
    fn rejects_wrong_length_fill_payload() {
        // The single arg here decodes to 3 bytes, not the 8 a fill index needs.
        assert!(decode(b"Gstoatty;fill;YWJj").is_none());
    }

    #[test]
    fn scroll_round_trips() {
        let command = ScrollCommand {
            page: 5_000_000_000,
            fraction: 40_000,
        };

        assert_eq!(
            decode(&encode_scroll(&command)),
            Some(Command::Scroll(command))
        );
    }

    #[test]
    fn rejects_wrong_length_scroll_payload() {
        // The single arg here decodes to 3 bytes, not the 10 a scroll offset needs.
        assert!(decode(b"Gstoatty;scroll;YWJj").is_none());
    }

    #[test]
    fn reset_round_trips() {
        assert_eq!(decode(&encode_reset()), Some(Command::Reset));
    }

    #[test]
    fn ignores_unknown_subcommand() {
        assert!(decode(b"Gstoatty;nope").is_none());
    }

    #[test]
    fn ignores_malformed_frame() {
        assert!(decode(b"garbage").is_none());
    }

    #[test]
    fn encode_into_round_trips_every_variant() {
        let commands = [
            Command::Border(BorderCommand {
                top: 1,
                left: 2,
                width: 3,
                height: 4,
                style: BorderStyle::Double,
                color: [9, 8, 7],
            }),
            Command::Scale(ScaleCommand {
                top: 5,
                left: 6,
                scale: 3,
            }),
            Command::Popover(PopoverCommand {
                top: 1,
                left: 1,
                width: 10,
                height: 4,
                fill: [10, 20, 30],
                border: [40, 50, 60],
                content_fg: [70, 80, 90],
                scale: 2,
                offset: [-3, 7],
                content: "hi".to_owned(),
            }),
            Command::ScrollRegion(ScrollRegionCommand {
                top: 2,
                left: 3,
                width: 8,
                height: 9,
                offset: 12,
            }),
            Command::Icon(IconCommand {
                top: 4,
                left: 5,
                kind: IconKind::Warning,
                color: [1, 2, 3],
                size: 2,
            }),
            Command::TextRun(TextRunCommand {
                col: -8,
                row: 16,
                scale: 256,
                color: [11, 22, 33],
                bg: [44, 55, 66],
                text: "42".to_owned(),
            }),
            Command::Bar(BarCommand {
                x: -4,
                y: 8,
                width: 6,
                height: 16,
                color: [200, 100, 50],
            }),
            Command::LineLayout(LineLayoutCommand {
                heights: vec![1, 2, 3, 1],
            }),
            Command::Fill(FillCommand { index: 7 }),
            Command::FillEnd,
            Command::Scroll(ScrollCommand {
                page: 12,
                fraction: 30_000,
            }),
            Command::Reset,
        ];

        for command in commands {
            let mut out = Vec::new();
            encode_into(&mut out, &command);
            assert_eq!(decode(&out), Some(command));
        }
    }

    #[test]
    fn encode_into_appends_each_frame() {
        let border = BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: BorderStyle::Light,
            color: [1, 1, 1],
        };
        let bar = BarCommand {
            x: 1,
            y: 1,
            width: 4,
            height: 8,
            color: [2, 2, 2],
        };

        let mut out = Vec::new();
        encode_into(&mut out, &Command::Border(border));
        encode_into(&mut out, &Command::Bar(bar));

        let mut expected = encode_border(&border);
        expected.extend(encode_bar(&bar));
        assert_eq!(out, expected);
    }
}
