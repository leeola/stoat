//! The typed command surface: a parsed [`Frame`] dispatched by its namespaced
//! sub-command.
//!
//! [`decode`] is the terminal-facing entry point. It returns `None` for any
//! frame the terminal should ignore, whether malformed or carrying a
//! sub-command this build does not recognize, so an unsupported feature
//! degrades to nothing rather than erroring.

use crate::frame::{self, Frame, FrameScratch};
use std::sync::Arc;

/// A decoded stoatty command.
///
/// The enum is intentionally exhaustive: adding a variant forces every matcher,
/// including the terminal's apply seam, to handle it rather than silently
/// dropping the new command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Border(BorderCommand),
    Panel(PanelCommand),
    Scale(ScaleCommand),
    /// Open a popover whose content text streams as the bytes that follow,
    /// until [`Command::PopoverEnd`] commits it. The fixed head (region, colors,
    /// scale, offset) rides on this marker; the content is captured off-frame so
    /// it is not bounded by the frame-size cap.
    Popover(PopoverCommand),
    /// Close the popover opened by [`Command::Popover`], committing the streamed
    /// content into its [`PopoverCommand::content`]. Carries no payload.
    PopoverEnd,
    ScrollRegion(ScrollRegionCommand),
    PoolRegion(PoolRegionCommand),
    Icon(IconCommand),
    /// Open a text run whose text streams as the bytes that follow, until
    /// [`Command::TextRunEnd`] commits it. The fixed head (position, scale,
    /// colors) rides on this marker; the text is captured off-frame so it is not
    /// bounded by the frame-size cap.
    TextRun(TextRunCommand),
    /// Close the text run opened by [`Command::TextRun`], committing the streamed
    /// text into its [`TextRunCommand::text`]. Carries no payload.
    TextRunEnd,
    Bar(BarCommand),
    Polyline(PolylineCommand),
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
    /// Anchor the primary cursor to the content while a pool glides.
    ///
    /// Carries the cursor's document row and grid column for pool
    /// [`PoolCursorCommand::pool`], so the terminal draws the cursor riding the
    /// eased scroll offset instead of easing it toward its last VT cell. Sent
    /// once per glide tick alongside the pool's [`Command::Scroll`] frame.
    PoolCursor(PoolCursorCommand),
    /// Jump the smooth-scroll target to a document page across an unbuffered gap.
    ///
    /// Re-anchors the live offset to a local neighbour of [`RepositionCommand`]'s
    /// page and lands softly on it, for a jump too far to ease across the pool
    /// window. Pair with the `fill`s that buffer the destination neighbourhood.
    Reposition(RepositionCommand),
    /// Retire a smooth-scroll pool, freeing the pages it buffered.
    ///
    /// Sent when the surface backing pool [`PoolDropCommand::pool`] goes away (a
    /// closed pane, a dismissed modal), so the terminal frees its region and page
    /// buffer rather than holding them for a pool that will never scroll again. A
    /// later [`Command::PoolRegion`] with the same id starts a fresh pool.
    PoolDrop(PoolDropCommand),
    /// Declare a minimap strip, a right-edge region rendering a whole buffer as
    /// colored per-line run blocks with a viewport thumb.
    ///
    /// A per-frame decoration cleared by [`Command::Reset`], like a border or a
    /// bar. The line run summaries it renders live in a persistent content store
    /// keyed by [`MinimapCommand::content_id`] and updated out of band by
    /// [`Command::MinimapLines`], so a redeclared strip keeps its content.
    Minimap(MinimapCommand),
    /// Splice per-line run summaries into a minimap content store.
    ///
    /// Persistent, unlike the [`Command::Minimap`] declaration: it survives
    /// [`Command::Reset`] and is retired only by [`Command::MinimapDrop`], so the
    /// program streams incremental line updates without resending the whole file.
    MinimapLines(MinimapLinesCommand),
    /// Update a minimap strip's viewport thumb position.
    ///
    /// Persistent state carrying only where the thumb sits, so a scroll moves the
    /// thumb with a small frame rather than re-declaring the strip.
    MinimapView(MinimapViewCommand),
    /// Retire a minimap content store, freeing the line summaries it held.
    ///
    /// Sent when the buffer backing [`MinimapDropCommand::content_id`] closes, so
    /// the terminal need not hold summaries for a minimap that will never render
    /// again.
    MinimapDrop(MinimapDropCommand),
    /// Open an aux OS window as a second render target for window-bound pools.
    WindowOpen(WindowOpenCommand),
    /// Close an aux OS window and free its render target.
    WindowClose(WindowCloseCommand),
    /// Raise and OS-focus an aux window.
    WindowFocus(WindowFocusCommand),
    /// Clear all accumulated stoatty decoration state, so the program can redraw
    /// its scene from scratch. Carries no payload.
    Reset,
    /// The terminal's own config file changed on disk, so it should re-read and
    /// re-apply it. Carries no payload: the terminal reads the file itself, and
    /// the program that sends this is only reporting that it wrote it.
    ConfigReload,
    /// The program claims the platform zoom combo for its session, or releases
    /// it.
    ///
    /// While claimed the terminal forwards each press upstream instead of
    /// stepping its own font size, so the program can make the combo mean
    /// whatever its current context calls for. A program that never sends this
    /// leaves the terminal's native font zoom exactly as it was, which is what
    /// keeps a plain shell child working and what a crashed program degrades
    /// back to.
    ZoomCapture {
        on: bool,
    },
    /// Step the terminal's font size by `delta`, positive to grow.
    ///
    /// The counterpart to claiming the combo. A program that took the combo
    /// away from font zoom uses this to offer font zoom back through its own
    /// commands.
    FontStep {
        delta: i32,
    },
    /// A handshake the program sends to identify itself to the terminal, so the
    /// terminal's log records which process drives it. The terminal replies with
    /// its own [`IdentReply`].
    Hello(HelloCommand),
}

/// The payload of [`Command::Hello`]: a program's self-identification.
///
/// Sent by a program (the stoat editor) to the terminal (stoatty) so the
/// terminal logs which process, over which session, on which host drives it --
/// the record that ties a remote editor to a local terminal log.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HelloCommand {
    pub pid: u32,
    pub log_id: String,
    pub hostname: String,
    pub version: String,
    /// The program's [`crate::PROTOCOL_VERSION`], or zero from one that predates
    /// the field. Distinct from `version`, which is the program's own package
    /// version and means nothing to the terminal beyond the log.
    pub protocol: u32,
}

/// The terminal's reply to a [`Command::Hello`], identifying the terminal.
///
/// Unlike a [`Command`], this travels terminal-to-program: it arrives as input
/// bytes on the program's stdin, so [`decode`] (the terminal-facing entry) never
/// yields it. Decode it explicitly with [`decode_ident_reply`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdentReply {
    pub pid: u32,
    pub log_id: String,
    pub hostname: String,
    pub version: String,
    /// The terminal's [`crate::PROTOCOL_VERSION`], or zero from one that
    /// predates the field. This is what an emitter gates a feature on, unlike
    /// `version`, which only names the terminal's own release.
    pub protocol: u32,
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
/// `corner_radius` device-pixel rounded corners (0 = square) and a `shadow`
/// drawn in the selected [`PanelShadow`] style. Unlike a per-cell
/// [`BorderCommand`], the frame is a floating component drawn under the grid
/// text, so the framed cells keep rendering their own content.
///
/// `fill` is [`Some`] to paint the interior that color, or [`None`] to leave the
/// cells' own SGR backgrounds showing through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// Device pixels shaved off each horizontal edge, so the box draws narrower
    /// than its cell rect. `0` is cell-exact. The border, fill, corner rounding,
    /// and shadow all follow the inset rect, leaving the strip outside it showing
    /// the cells behind.
    pub inset_x: u8,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it.
    pub above_pools: bool,
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
    /// Shape the content text at bold weight rather than the default. Only the
    /// content is affected. The box chrome is unchanged.
    pub bold: bool,
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

/// Declare the sub-rectangle a smooth-scroll document pool composites into.
///
/// The pool is `width` by `height` cells with its top-left at (`top`, `left`) in
/// absolute grid coordinates. Unlike [`ScrollRegionCommand`] it carries no
/// offset: the pool's scroll position rides [`ScrollCommand`] (page plus
/// fraction). The renderer composites the eased pool over this rectangle and
/// draws the rest of the grid -- any static chrome around it -- from the live
/// content, so a program need not own the whole viewport to smooth-scroll.
///
/// `pool` names which pool this declares. Pools scroll independently and
/// composite in ascending-id z-order, so a program can smooth-scroll several
/// regions at once (split panes side by side, a modal stacked over an editor).
/// Re-declaring an existing id updates that pool's rectangle;
/// [`Command::PoolDrop`] retires it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PoolRegionCommand {
    pub pool: u32,
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    /// Which OS window the pool renders into. `0` is the primary grid, where the
    /// region's coordinates are grid-absolute. A nonzero `N` binds the pool to
    /// aux window `N`, where the coordinates are relative to that window's own
    /// grid.
    pub window: u32,
}

/// First pool id reserved for non-pane surfaces. Split-pane editor pools
/// occupy `[1, NON_PANE_POOL_BASE)`.
///
/// The two id ranges also encode a z-relationship the renderer relies on when
/// it composites pools against modal boxes. A pool below the base is
/// editor-pane content that sits *under* every box, so its eased composite is
/// occluded by any box it slides beneath. A pool at or above the base is a
/// box's own content, such as a finder or palette list easing, so it is never
/// occluded.
///
/// Shared here because the pool producer (stoat) and the compositor (stoatty)
/// must agree on the split.
pub const NON_PANE_POOL_BASE: u32 = 1 << 24;

/// Open an aux OS window `cols` by `rows` cells with an initial `title`.
///
/// The terminal creates a native window as a second render target for
/// window-bound pools, those whose [`PoolRegionCommand::window`] names it. The
/// primary grid is window `0` and is never opened this way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WindowOpenCommand {
    pub window: u32,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
}

/// Close the aux OS window named by [`WindowCloseCommand::window`].
///
/// The terminal destroys the native window and frees its render target, so any
/// pools still bound to it stop compositing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowCloseCommand {
    pub window: u32,
}

/// Raise and OS-focus the aux window named by [`WindowFocusCommand::window`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowFocusCommand {
    pub window: u32,
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
/// `bg`, when `Some`, is an opaque background box the renderer paints across the
/// run's full width (spaces included) before the glyphs alpha-blend over it, so
/// it need not match whatever lies beneath. `None` draws the glyphs with no
/// backing box, blending them directly over the surface behind the run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextRunCommand {
    pub col: i16,
    pub row: i16,
    pub scale: u16,
    pub color: [u8; 3],
    pub bg: Option<[u8; 3]>,
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

/// A stroked path drawn off the cell grid in a solid color.
///
/// The protocol's only non-axis-aligned primitive, added for the commit
/// graph's lane and merge lines. Every coordinate and [`Self::width`] is in
/// **sixteenths of a cell** like [`BarCommand`], so a path tracks live font
/// zoom.
///
/// A single point, or two equal points, is legal and draws a dot. That is how
/// a graph marks a commit node without a second primitive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PolylineCommand {
    /// Vertices in draw order, each `[x, y]`. Consecutive pairs are the
    /// segments. An empty list draws nothing.
    pub points: Vec<[i16; 2]>,
    /// Stroke thickness, centered on the path, in sixteenths of the cell's
    /// width. Measured against the width on both axes, so a diagonal is as
    /// thick as a vertical and 16 draws exactly one column wide.
    pub width: u16,
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

/// Name the pool and document page a [`Command::Fill`] redirect paints into.
///
/// The open half of the `fill`/`fill_end` marker pair. A page is a full grid of
/// cells, far larger than the APC frame cap, so it cannot ride a frame payload:
/// this marker only names the target page, and the page's content streams as
/// ordinary VT + SGR bytes after the frame, committed when the redirect closes.
/// `pool` selects which pool's buffer receives the page; `index` is the app's
/// document page index, the same key the pool slot is addressed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FillCommand {
    pub pool: u32,
    pub index: u64,
}

/// A smooth-scroll target as a document-page offset.
///
/// Names where the program wants pool [`Self::pool`]'s viewport: `page` is the
/// document page index (the same key the page pool is addressed by) and
/// `fraction` is the sub-page position within it, in 1/65536ths of a page. The
/// renderer eases the live offset toward this position rather than jumping, so
/// the program reports an absolute target and the terminal animates toward it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollCommand {
    pub pool: u32,
    pub page: u64,
    pub fraction: u16,
}

/// A pool's primary-cursor anchor, tracked for the duration of a glide.
///
/// While pool [`Self::pool`] eases toward a scroll target, the cursor rides the
/// content rather than the VT grid. `row` is the document display row the cursor
/// sits on, and `col` is its grid-absolute column. The terminal draws the cursor
/// at region row `row` minus the pool's eased document offset, hiding it while
/// that row lands outside the region, instead of easing the cursor toward its
/// last VT cell.
///
/// Pairs with [`ScrollCommand`], which moves the same pool's viewport. The anchor
/// ships once per glide tick, so the drawn cursor stays frame-locked to the eased
/// content offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolCursorCommand {
    pub pool: u32,
    pub row: u64,
    pub col: u16,
}

/// A discontinuous smooth-scroll jump to a document page.
///
/// `page` is the destination document page index in pool [`Self::pool`]. Unlike
/// [`ScrollCommand`], which the terminal eases toward across the buffered window,
/// this re-anchors the live offset to a local neighbour of the destination and
/// lands softly on it, so a jump too far to animate within the pool does not drag
/// across the unbuffered gap. The program pushes a window of pages around the
/// destination before sending it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RepositionCommand {
    pub pool: u32,
    pub page: u64,
}

/// Retire smooth-scroll pool [`Self::pool`], freeing the pages it buffered.
///
/// The payload of [`Command::PoolDrop`]: a single pool id. Sent when the surface
/// backing the pool goes away, so the terminal need not hold its buffers for a
/// pool that will never scroll again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolDropCommand {
    pub pool: u32,
}

/// A single colored run on one minimap line, `len` columns wide starting at
/// `start_col`, drawn in palette entry `class`.
///
/// Columns and lengths are minimap columns (0 to `max_columns`), and `class`
/// indexes the strip's declared palette, so a run names color by class rather
/// than carrying an rgb triple per run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapRun {
    pub start_col: u8,
    pub len: u8,
    pub class: u8,
}

/// The run summary of one buffer line, its runs left to right.
///
/// Empty for a blank line. The emitter caps the run count per line, and the wire
/// format bounds it to 255.
///
/// Shared rather than owned because a line is handed on several times without
/// ever being changed. It goes from the decode into the terminal's store, from
/// there into the grid's, and again wholesale whenever a resize reclones the
/// store. A file's worth of lines copied at each of those is what sharing them
/// avoids.
pub type LineSummary = Arc<[MinimapRun]>;

/// Declare a minimap strip and its rendering parameters.
///
/// The payload of [`Command::Minimap`]. The strip occupies a `width` by `height`
/// cell region at (`top`, `left`); [`Self::strip_id`] names this declaration and
/// [`Self::content_id`] the line-summary store it renders (see
/// [`MinimapLinesCommand`]). Each summarized line is drawn `lines_per_cell` to a
/// cell, up to `max_columns` wide. [`Self::bg`] and [`Self::thumb`] are rgba, the
/// thumb being the viewport overlay; [`Self::thumb_border`] is its rgb outline.
/// [`Self::palette`] holds up to 64 rgb entries a run's class indexes.
///
/// The palette is generic over its container so an emitter declaring a strip per
/// frame can hand a borrowed slice, while a decoded command owns its copy. It
/// defaults to the owned form, so a holder that is not encoding writes the type
/// with no parameter.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapCommand<P = Vec<[u8; 3]>> {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub strip_id: u32,
    pub content_id: u32,
    pub lines_per_cell: u8,
    pub max_columns: u8,
    pub bg: [u8; 4],
    pub thumb: [u8; 4],
    pub thumb_border: [u8; 3],
    pub palette: P,
}

/// Splice line summaries into the content store [`Self::content_id`].
///
/// The payload of [`Command::MinimapLines`]. Starting at line [`Self::start`], it
/// replaces [`Self::removed`] existing lines with [`Self::lines`]. A pure
/// deletion carries an empty [`Self::lines`]. A pure insertion carries a zero
/// [`Self::removed`]. The wire count of inserted lines is `lines.len()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapLinesCommand {
    pub content_id: u32,
    pub start: u32,
    pub removed: u32,
    pub lines: Vec<LineSummary>,
}

/// Position a minimap strip's viewport thumb.
///
/// The payload of [`Command::MinimapView`]. [`Self::strip_id`] selects the strip;
/// [`Self::top_256`] is the fractional top buffer line in 1/256ths of a line, and
/// [`Self::visible_lines`] the viewport height in lines, together sizing and
/// placing the thumb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapViewCommand {
    pub strip_id: u32,
    pub top_256: u32,
    pub visible_lines: u16,
}

/// Retire the minimap content store [`Self::content_id`].
///
/// The payload of [`Command::MinimapDrop`]: a single content-store id, dropped
/// when its backing buffer closes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapDropCommand {
    pub content_id: u32,
}

/// Decode a stoatty APC frame into a typed [`Command`], or `None` to ignore it.
///
/// `None` covers both a malformed frame and a well-formed one whose
/// sub-command is unknown to this build. Ignoring rather than erroring is what
/// lets the same byte stream degrade to nothing in another terminal.
pub fn decode(bytes: &[u8]) -> Option<Command> {
    let mut scratch = FrameScratch::default();
    decode_with(bytes, &mut scratch)
}

/// Decode a stoatty command, reusing `scratch`'s argument buffers.
///
/// Behaves like [`decode`] but takes a caller-owned [`FrameScratch`] so a hot
/// decode loop reuses the per-argument buffers across frames instead of
/// allocating fresh ones each call.
pub fn decode_with(bytes: &[u8], scratch: &mut FrameScratch) -> Option<Command> {
    let (sub, args) = frame::decode_into(bytes, scratch)?;
    dispatch(sub, args)
}

/// Decode every stoatty frame in an emitted byte stream, in emission order.
///
/// This is what an emitter asserts its own output with. [`decode`] reads one
/// frame, and a program that emits a scene produces a run of them mixed with
/// content that rides outside the wrapper, so reading a scene back needs a walk
/// rather than a parse.
///
/// [`Command::TextRun`] and [`Command::Popover`] stream their text between the
/// open frame and the matching `_end`, so [`decode`] alone yields them empty.
/// This stitches those bytes back onto the command from the gap that follows
/// the open frame. The gap ends at the next frame introducer, not at the first
/// `ESC`, so a `fill` batch works too: a page's VT carries its own escape
/// sequences, and the walk steps over them to the `fill_end` rather than
/// stopping mid-page.
///
/// Anything between frames that no capture claims is skipped, as is a frame
/// this build does not recognize and one that is malformed. A trailing frame
/// with no terminator ends the walk, since nothing later in the stream belongs
/// to a frame that never closed.
pub fn decode_stream(bytes: &[u8]) -> Vec<Command> {
    let mut out = Vec::new();
    let mut scratch = FrameScratch::default();
    let mut rest = bytes;

    while let Some(span) = frame::apc_span(rest) {
        let decoded = decode_with(&rest[span.clone()], &mut scratch);
        rest = &rest[span.end..];

        match decoded {
            Some(Command::TextRun(mut command)) => {
                command.text = take_streamed(&mut rest);
                out.push(Command::TextRun(command));
            },
            Some(Command::Popover(mut command)) => {
                command.content = take_streamed(&mut rest);
                out.push(Command::Popover(command));
            },
            Some(command) => out.push(command),
            None => {},
        }
    }

    out
}

/// Split the streamed content off the front of `rest`, leaving it at the next
/// frame.
///
/// Lossy because a capture is plain text by contract, so bytes that are not
/// valid UTF-8 mean the emitter broke that contract. Reading them as
/// replacement characters still shows the caller what arrived. Dropping the
/// whole command on a decode failure shows nothing at all.
fn take_streamed(rest: &mut &[u8]) -> String {
    let end = frame::apc_span(rest).map_or(rest.len(), |span| span.start);
    let content = String::from_utf8_lossy(&rest[..end]).into_owned();
    *rest = &rest[end..];
    content
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
        Ok(())
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
        command.bold,
        &command.content,
    );
    out
}

/// Append a `Gstoatty;popover` open marker, its streamed `content`, and a
/// `Gstoatty;popover_end` close marker to `out`.
///
/// The fixed head fields ride in the open marker's single argument; `content`
/// streams as the raw bytes between the two markers, so it is not bounded by the
/// per-frame size cap. `content` is borrowed so an emitter can pass a slice of
/// its own buffer rather than build an owned [`String`] per frame.
///
/// `content` must be plain text. Riding outside the APC wrapper is what frees
/// it from the size cap and is also what makes it indistinguishable from
/// terminal control bytes, so the terminal cuts the capture at the first `ESC`
/// and keeps only what came before. An emitter that wants styled content sets
/// the colors in the head fields rather than with escape sequences in the text.
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
    bold: bool,
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
        w.write_all(&offset[1].to_be_bytes())?;
        w.write_all(&[bold as u8])
    });
    frame::end(out);
    out.extend_from_slice(content.as_bytes());
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

/// Encode a [`PoolRegionCommand`] as a full `Gstoatty;pool_region` frame for an
/// emitter.
pub fn encode_pool_region(command: &PoolRegionCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_region_into(&mut out, command);
    out
}

/// Append a `Gstoatty;pool_region` frame for `command` to `out` without
/// allocating.
pub fn encode_pool_region_into(out: &mut Vec<u8>, command: &PoolRegionCommand) {
    frame::begin(out, "pool_region");
    frame::push_arg(out, |w| {
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.window.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`WindowOpenCommand`] as a full `Gstoatty;window_open` frame.
pub fn encode_window_open(command: &WindowOpenCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_open_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_open` frame for `command` to `out`.
pub fn encode_window_open_into(out: &mut Vec<u8>, command: &WindowOpenCommand) {
    frame::begin(out, "window_open");
    frame::push_arg(out, |w| {
        w.write_all(&command.window.to_be_bytes())?;
        w.write_all(&command.cols.to_be_bytes())?;
        w.write_all(&command.rows.to_be_bytes())
    });
    frame::push_arg(out, |w| w.write_all(command.title.as_bytes()));
    frame::end(out);
}

/// Encode a [`WindowCloseCommand`] as a full `Gstoatty;window_close` frame.
pub fn encode_window_close(command: &WindowCloseCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_close_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_close` frame for `command` to `out`.
pub fn encode_window_close_into(out: &mut Vec<u8>, command: &WindowCloseCommand) {
    frame::begin(out, "window_close");
    frame::push_arg(out, |w| w.write_all(&command.window.to_be_bytes()));
    frame::end(out);
}

/// Encode a [`WindowFocusCommand`] as a full `Gstoatty;window_focus` frame.
pub fn encode_window_focus(command: &WindowFocusCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_focus_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_focus` frame for `command` to `out`.
pub fn encode_window_focus_into(out: &mut Vec<u8>, command: &WindowFocusCommand) {
    frame::begin(out, "window_focus");
    frame::push_arg(out, |w| w.write_all(&command.window.to_be_bytes()));
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
        w.write_all(&[command.size])?;
        w.write_all(&command.offset[0].to_be_bytes())?;
        w.write_all(&command.offset[1].to_be_bytes())
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
/// Append a `Gstoatty;text_run` open marker, its streamed `text`, and a
/// `Gstoatty;text_run_end` close marker to `out`.
///
/// The fixed head fields ride in the open marker's single argument; `text`
/// streams as the raw bytes between the two markers, so it is not bounded by the
/// per-frame size cap. `text` is borrowed so an emitter can pass a slice of a
/// reused buffer (a gutter formats line numbers into a stack buffer) rather than
/// build an owned [`String`] per frame.
///
/// `text` must be plain text. Riding outside the APC wrapper is what frees it
/// from the size cap and is also what makes it indistinguishable from terminal
/// control bytes, so the terminal cuts the capture at the first `ESC` and keeps
/// only what came before. An emitter that wants a styled run sets `color` and
/// `bg` rather than writing escape sequences into the text.
pub fn encode_text_run_into(
    out: &mut Vec<u8>,
    col: i16,
    row: i16,
    scale: u16,
    color: [u8; 3],
    bg: Option<[u8; 3]>,
    text: &str,
) {
    frame::begin(out, "text_run");
    frame::push_arg(out, |w| {
        w.write_all(&col.to_be_bytes())?;
        w.write_all(&row.to_be_bytes())?;
        w.write_all(&scale.to_be_bytes())?;
        w.write_all(&color)?;
        w.write_all(&bg.unwrap_or([0, 0, 0]))?;
        w.write_all(&[bg.is_some() as u8])
    });
    frame::end(out);
    out.extend_from_slice(text.as_bytes());
    encode_text_run_end_into(out);
}

/// Encode a [`Command::TextRunEnd`] as a full `Gstoatty;text_run_end`
/// close-marker frame.
///
/// The frame carries no arguments; receiving it commits the text streamed since
/// the matching [`Command::TextRun`] into the run's `text`.
pub fn encode_text_run_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_text_run_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;text_run_end` close-marker frame to `out`.
pub fn encode_text_run_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "text_run_end");
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

/// Encode a [`PolylineCommand`] as a full `Gstoatty;polyline` frame for an
/// emitter.
pub fn encode_polyline(command: &PolylineCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_polyline_into(&mut out, command);
    out
}

/// Append a `Gstoatty;polyline` frame for `command` to `out` without
/// allocating.
///
/// The stroke head and the point list ride in one argument. Width and color
/// come first, then each vertex as a pair of big-endian `i16`s. The points are
/// streamed straight into the base64 sink rather than through an intermediate
/// buffer.
///
/// One frame carries at most 12283 points. The terminal's scanner drops a
/// payload past [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so
/// one point too many loses the whole path silently. Split a longer path into
/// several polylines that repeat the vertex they meet at, which draws as one
/// continuous stroke. Passing more panics in debug and emits a frame the
/// terminal discards in release.
pub fn encode_polyline_into(out: &mut Vec<u8>, command: &PolylineCommand) {
    let start = out.len();

    frame::begin(out, "polyline");
    frame::push_arg(out, |w| {
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.color)?;
        for point in &command.points {
            w.write_all(&point[0].to_be_bytes())?;
            w.write_all(&point[1].to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);

    debug_assert!(
        frame::payload_len(out.len() - start) <= frame::MAX_APC_PAYLOAD,
        "a {}-point polyline overruns the frame cap; split it into paths that \
         share the vertex they meet at",
        command.points.len(),
    );
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
///
/// One frame carries at most 24567 heights. The terminal's scanner drops a
/// payload past [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so
/// one height too many loses the entire layout with nothing on screen to say
/// so. A layout is replaced whole and has no split form, unlike a `minimap_lines`
/// splice, so an emitter with a longer surface has to send the window it
/// declares rather than the whole document. Passing more panics in debug and
/// emits a frame the terminal discards in release.
pub fn encode_line_layout_into(out: &mut Vec<u8>, heights: &[u16]) {
    let start = out.len();

    frame::begin(out, "line_layout");
    frame::push_arg(out, |w| {
        for height in heights {
            w.write_all(&height.to_be_bytes())?;
        }
        Ok(())
    });
    frame::end(out);

    debug_assert!(
        frame::payload_len(out.len() - start) <= frame::MAX_APC_PAYLOAD,
        "a {}-height line_layout overruns the frame cap; declare the visible \
         window instead, since a layout has no split form",
        heights.len(),
    );
}

/// Encode a [`FillCommand`] as a full `Gstoatty;fill` open-marker frame.
///
/// The page index rides in a single fixed 8-byte big-endian argument; the
/// page's content streams as VT bytes after the frame, not as a frame argument.
pub fn encode_fill(command: &FillCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_fill_into(&mut out, command.pool, command.index);
    out
}

/// Append a `Gstoatty;fill` open-marker frame for page `index` of pool `pool`
/// to `out`.
pub fn encode_fill_into(out: &mut Vec<u8>, pool: u32, index: u64) {
    frame::begin(out, "fill");
    frame::push_arg(out, |w| {
        w.write_all(&pool.to_be_bytes())?;
        w.write_all(&index.to_be_bytes())
    });
    frame::end(out);
}

/// The `(pool, index)` a batch of bytes fills, or `None` when it is not a fill.
///
/// A fill batch is self-contained, holding the open marker, the page's VT
/// bytes, and the close marker. Only the marker names the page, so this decodes
/// the first frame and stops, never walking the page content behind it.
///
/// The point of naming it is that pool slots are last-writer-wins and no other
/// command reads a page's content, so a queued fill for a key that appears
/// again later is work nobody will ever see. A sender with a backlog can drop
/// the earlier one. `None` covers everything that has no such guarantee, from
/// a different command to bytes that do not open with a frame at all.
pub fn fill_batch_key(batch: &[u8]) -> Option<(u32, u64)> {
    let end = frame::first_frame_end(batch)?;
    let frame = frame::decode(&batch[..end])?;
    match frame.sub.as_str() {
        "fill" => decode_fill(&frame.args).map(|fill| (fill.pool, fill.index)),
        _ => None,
    }
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
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.page.to_be_bytes())?;
        w.write_all(&command.fraction.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`PoolCursorCommand`] as a full `Gstoatty;pool_cursor` frame.
///
/// The cursor anchor rides one fixed 14-byte big-endian argument holding the
/// pool, row, and column, the same shape as [`encode_scroll`]'s target.
pub fn encode_pool_cursor(command: &PoolCursorCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_cursor_into(&mut out, command);
    out
}

/// Append a `Gstoatty;pool_cursor` frame for `command` to `out` without allocating.
pub fn encode_pool_cursor_into(out: &mut Vec<u8>, command: &PoolCursorCommand) {
    frame::begin(out, "pool_cursor");
    frame::push_arg(out, |w| {
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.row.to_be_bytes())?;
        w.write_all(&command.col.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`RepositionCommand`] as a full `Gstoatty;reposition` frame.
///
/// The destination page index rides in a single fixed 8-byte big-endian
/// argument, the same shape as [`encode_fill`]'s page index.
pub fn encode_reposition(command: &RepositionCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_reposition_into(&mut out, command.pool, command.page);
    out
}

/// Append a `Gstoatty;reposition` frame for destination `page` of pool `pool`
/// to `out`.
pub fn encode_reposition_into(out: &mut Vec<u8>, pool: u32, page: u64) {
    frame::begin(out, "reposition");
    frame::push_arg(out, |w| {
        w.write_all(&pool.to_be_bytes())?;
        w.write_all(&page.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`PoolDropCommand`] as a full `Gstoatty;pool_drop` frame for an
/// emitter.
pub fn encode_pool_drop(command: &PoolDropCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_drop_into(&mut out, command.pool);
    out
}

/// Append a `Gstoatty;pool_drop` frame retiring pool `pool` to `out`.
pub fn encode_pool_drop_into(out: &mut Vec<u8>, pool: u32) {
    frame::begin(out, "pool_drop");
    frame::push_arg(out, |w| w.write_all(&pool.to_be_bytes()));
    frame::end(out);
}

/// Encode a [`MinimapCommand`] as a full `Gstoatty;minimap` frame for an emitter.
///
/// The fixed head rides in a 29-byte first argument and the palette in a second
/// argument of consecutive rgb triples.
pub fn encode_minimap<P: AsRef<[[u8; 3]]>>(command: &MinimapCommand<P>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap` frame for `command` to `out`.
pub fn encode_minimap_into<P: AsRef<[[u8; 3]]>>(out: &mut Vec<u8>, command: &MinimapCommand<P>) {
    frame::begin(out, "minimap");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.strip_id.to_be_bytes())?;
        w.write_all(&command.content_id.to_be_bytes())?;
        w.write_all(&[command.lines_per_cell, command.max_columns])?;
        w.write_all(&command.bg)?;
        w.write_all(&command.thumb)?;
        w.write_all(&command.thumb_border)
    });
    frame::push_arg(out, |w| {
        for entry in command.palette.as_ref() {
            w.write_all(entry)?;
        }
        Ok(())
    });
    frame::end(out);
}

/// Raw argument bytes one `minimap_lines` frame may carry.
///
/// Base64 turns 3 bytes into 4, so the pre-image of a full payload is three
/// quarters of [`frame::MAX_APC_PAYLOAD`]. The 64 subtracted covers the
/// `Gstoatty;minimap_lines;` prefix, which shares the payload budget with the
/// argument, plus margin.
const MINIMAP_LINES_RAW_BUDGET: usize = (frame::MAX_APC_PAYLOAD - 64) / 4 * 3;

/// Bytes the fixed fields take ahead of the line data in a splice argument.
/// Four `u32`s carry the content id, start, removed count, and line count.
const MINIMAP_LINES_HEADER: usize = 16;

/// Encode a [`MinimapLinesCommand`] as `Gstoatty;minimap_lines` frames.
///
/// A splice too large for one APC payload is spread over several frames, so the
/// result is one frame for an ordinary splice and a run of them for a big one.
/// See [`encode_minimap_lines_into`] for what the split guarantees.
pub fn encode_minimap_lines(command: &MinimapLinesCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_lines_into(&mut out, command);
    out
}

/// Append the `Gstoatty;minimap_lines` frames for `command` to `out`.
///
/// The terminal's scanner discards an APC payload past
/// [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so a splice whose
/// lines would overrun that is packed into as many frames as it takes. Applying
/// them in order has the same effect as applying `command` in one go. The first
/// frame carries the removal and the leading lines, and each later one inserts
/// its share at the point the previous frame stopped.
///
/// A splice that fits emits exactly one frame, as does a pure deletion.
pub fn encode_minimap_lines_into(out: &mut Vec<u8>, command: &MinimapLinesCommand) {
    let budget = MINIMAP_LINES_RAW_BUDGET - MINIMAP_LINES_HEADER;
    let mut emitted = 0;
    loop {
        let mut used = 0;
        let mut take = 0;
        for line in &command.lines[emitted..] {
            let cost = 1 + 3 * line.len();
            // Always take one line, so a hypothetical line past the whole
            // budget still advances rather than looping forever. A line maxes
            // out at 1 + 3 * 255 bytes, far under it.
            if take > 0 && used + cost > budget {
                break;
            }
            used += cost;
            take += 1;
        }

        write_minimap_lines_frame(
            out,
            command.content_id,
            command.start + emitted as u32,
            if emitted == 0 { command.removed } else { 0 },
            &command.lines[emitted..emitted + take],
        );

        emitted += take;
        if emitted >= command.lines.len() {
            return;
        }
    }
}

/// Append one `Gstoatty;minimap_lines` frame splicing `lines` in at `start`.
fn write_minimap_lines_frame(
    out: &mut Vec<u8>,
    content_id: u32,
    start: u32,
    removed: u32,
    lines: &[LineSummary],
) {
    frame::begin(out, "minimap_lines");
    frame::push_arg(out, |w| {
        w.write_all(&content_id.to_be_bytes())?;
        w.write_all(&start.to_be_bytes())?;
        w.write_all(&removed.to_be_bytes())?;
        w.write_all(&(lines.len() as u32).to_be_bytes())?;
        for line in lines {
            w.write_all(&[line.len() as u8])?;
            for run in line.iter() {
                w.write_all(&[run.start_col, run.len, run.class])?;
            }
        }
        Ok(())
    });
    frame::end(out);
}

/// Encode a [`MinimapViewCommand`] as a full `Gstoatty;minimap_view` frame.
///
/// The strip id, fractional top, and viewport height ride in a fixed 10-byte
/// argument.
pub fn encode_minimap_view(command: &MinimapViewCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_view_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap_view` frame for `command` to `out`.
pub fn encode_minimap_view_into(out: &mut Vec<u8>, command: &MinimapViewCommand) {
    frame::begin(out, "minimap_view");
    frame::push_arg(out, |w| {
        w.write_all(&command.strip_id.to_be_bytes())?;
        w.write_all(&command.top_256.to_be_bytes())?;
        w.write_all(&command.visible_lines.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`MinimapDropCommand`] as a full `Gstoatty;minimap_drop` frame.
pub fn encode_minimap_drop(command: &MinimapDropCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_drop_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap_drop` frame retiring content store `content_id`.
pub fn encode_minimap_drop_into(out: &mut Vec<u8>, command: &MinimapDropCommand) {
    frame::begin(out, "minimap_drop");
    frame::push_arg(out, |w| w.write_all(&command.content_id.to_be_bytes()));
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

/// Encode a [`Command::ConfigReload`] as a full `Gstoatty;config_reload` frame
/// for an emitter.
pub fn encode_config_reload() -> Vec<u8> {
    let mut out = Vec::new();
    encode_config_reload_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;config_reload` frame to `out`.
pub fn encode_config_reload_into(out: &mut Vec<u8>) {
    frame::begin(out, "config_reload");
    frame::end(out);
}

/// Encode a [`Command::ZoomCapture`] as a full `Gstoatty;zoom_capture` frame for
/// an emitter.
pub fn encode_zoom_capture(on: bool) -> Vec<u8> {
    let mut out = Vec::new();
    encode_zoom_capture_into(&mut out, on);
    out
}

/// Append a `Gstoatty;zoom_capture` frame for `on` to `out`.
///
/// The claim rides as the word `on` or `off` rather than a byte, since it is a
/// once-per-session handshake and a readable frame is worth more there than a
/// byte saved.
pub fn encode_zoom_capture_into(out: &mut Vec<u8>, on: bool) {
    frame::begin(out, "zoom_capture");
    frame::push_arg(out, |w| w.write_all(if on { b"on" } else { b"off" }));
    frame::end(out);
}

/// Encode a [`Command::FontStep`] as a full `Gstoatty;font_step` frame for an
/// emitter.
pub fn encode_font_step(delta: i32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_font_step_into(&mut out, delta);
    out
}

/// Append a `Gstoatty;font_step` frame for `delta` to `out`.
pub fn encode_font_step_into(out: &mut Vec<u8>, delta: i32) {
    frame::begin(out, "font_step");
    frame::push_arg(out, |w| write!(w, "{delta}"));
    frame::end(out);
}

/// Encode a [`HelloCommand`] as a full `Gstoatty;hello` frame for an emitter.
pub fn encode_hello(command: &HelloCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_hello_into(&mut out, command);
    out
}

/// Append a `Gstoatty;hello` frame for `command` to `out`.
///
/// The fields ride as separate arguments in field order, with the numbers as
/// decimal strings so the frame stays legible in a raw log. `protocol` comes
/// last because it was appended after the rest.
pub fn encode_hello_into(out: &mut Vec<u8>, command: &HelloCommand) {
    frame::begin(out, "hello");
    frame::push_arg(out, |w| write!(w, "{}", command.pid));
    frame::push_arg(out, |w| w.write_all(command.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.version.as_bytes()));
    frame::push_arg(out, |w| write!(w, "{}", command.protocol));
    frame::end(out);
}

fn decode_zoom_capture(args: &[Vec<u8>]) -> Option<Command> {
    let [on, ..] = args else {
        return None;
    };
    match on.as_slice() {
        b"on" => Some(Command::ZoomCapture { on: true }),
        b"off" => Some(Command::ZoomCapture { on: false }),
        _ => None,
    }
}

fn decode_font_step(args: &[Vec<u8>]) -> Option<Command> {
    let [delta, ..] = args else {
        return None;
    };
    Some(Command::FontStep {
        delta: std::str::from_utf8(delta).ok()?.parse().ok()?,
    })
}

fn decode_hello(args: &[Vec<u8>]) -> Option<HelloCommand> {
    let [pid, log_id, hostname, version, ..] = args else {
        return None;
    };
    Some(HelloCommand {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
        protocol: decode_protocol(args.get(4)),
    })
}

/// Encode an [`IdentReply`] as a full `Gstoatty;ident` frame for a terminal.
pub fn encode_ident_reply(reply: &IdentReply) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ident_reply_into(&mut out, reply);
    out
}

/// Append a `Gstoatty;ident` frame for `reply` to `out`.
///
/// The terminal writes this to the program's stdin in answer to a
/// [`Command::Hello`], carrying the same fields.
pub fn encode_ident_reply_into(out: &mut Vec<u8>, reply: &IdentReply) {
    frame::begin(out, "ident");
    frame::push_arg(out, |w| write!(w, "{}", reply.pid));
    frame::push_arg(out, |w| w.write_all(reply.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.version.as_bytes()));
    frame::push_arg(out, |w| write!(w, "{}", reply.protocol));
    frame::end(out);
}

/// Decode a `Gstoatty;ident` [`Frame`] into an [`IdentReply`], or `None` when it
/// is not an ident frame or its fields do not parse.
///
/// Separate from [`decode`] because an ident reply travels terminal-to-program
/// and never appears in the terminal-facing command stream.
pub fn decode_ident_reply(frame: &Frame) -> Option<IdentReply> {
    if frame.sub != "ident" {
        return None;
    }
    let [pid, log_id, hostname, version, ..] = frame.args.as_slice() else {
        return None;
    };
    Some(IdentReply {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
        protocol: decode_protocol(frame.args.get(4)),
    })
}

/// The protocol version an appended handshake argument carries, or zero when it
/// is absent or unreadable.
///
/// A terminal built before the field existed sends no fifth argument at all, and
/// reports as version zero rather than failing the handshake over it.
fn decode_protocol(arg: Option<&Vec<u8>>) -> u32 {
    arg.and_then(|arg| std::str::from_utf8(arg).ok()?.parse().ok())
        .unwrap_or(0)
}

/// Append the full `Gstoatty` frame for any [`Command`] to `out` without
/// allocating, dispatching on the variant.
///
/// The encode-side mirror of [`decode`]: an emitter assembling a scene appends
/// each command into one reused buffer.
pub fn encode_into(out: &mut Vec<u8>, command: &Command) {
    match command {
        Command::Border(c) => encode_border_into(out, c),
        Command::Panel(c) => encode_panel_into(out, c),
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
            c.bold,
            &c.content,
        ),
        Command::PopoverEnd => encode_popover_end_into(out),
        Command::ScrollRegion(c) => encode_scroll_region_into(out, c),
        Command::PoolRegion(c) => encode_pool_region_into(out, c),
        Command::Icon(c) => encode_icon_into(out, c),
        Command::TextRun(c) => {
            encode_text_run_into(out, c.col, c.row, c.scale, c.color, c.bg, &c.text)
        },
        Command::TextRunEnd => encode_text_run_end_into(out),
        Command::Bar(c) => encode_bar_into(out, c),
        Command::Polyline(c) => encode_polyline_into(out, c),
        Command::LineLayout(c) => encode_line_layout_into(out, &c.heights),
        Command::Fill(c) => encode_fill_into(out, c.pool, c.index),
        Command::FillEnd => encode_fill_end_into(out),
        Command::Scroll(c) => encode_scroll_into(out, c),
        Command::PoolCursor(c) => encode_pool_cursor_into(out, c),
        Command::Reposition(c) => encode_reposition_into(out, c.pool, c.page),
        Command::PoolDrop(c) => encode_pool_drop_into(out, c.pool),
        Command::Minimap(c) => encode_minimap_into(out, c),
        Command::MinimapLines(c) => encode_minimap_lines_into(out, c),
        Command::MinimapView(c) => encode_minimap_view_into(out, c),
        Command::MinimapDrop(c) => encode_minimap_drop_into(out, c),
        Command::WindowOpen(c) => encode_window_open_into(out, c),
        Command::WindowClose(c) => encode_window_close_into(out, c),
        Command::WindowFocus(c) => encode_window_focus_into(out, c),
        Command::Reset => encode_reset_into(out),
        Command::ConfigReload => encode_config_reload_into(out),
        Command::ZoomCapture { on } => encode_zoom_capture_into(out, *on),
        Command::FontStep { delta } => encode_font_step_into(out, *delta),
        Command::Hello(c) => encode_hello_into(out, c),
    }
}

/// Map a sub-command name and its decoded arguments to a [`Command`].
///
/// An unknown sub-command, or a known one whose payload does not parse, yields
/// `None` so the frame is ignored.
fn dispatch(sub: &str, args: &[Vec<u8>]) -> Option<Command> {
    match sub {
        "border" => decode_border(args).map(Command::Border),
        "panel" => decode_panel(args).map(Command::Panel),
        "scale" => decode_scale(args).map(Command::Scale),
        "popover" => decode_popover(args).map(Command::Popover),
        "popover_end" => Some(Command::PopoverEnd),
        "scroll_region" => decode_scroll_region(args).map(Command::ScrollRegion),
        "pool_region" => decode_pool_region(args).map(Command::PoolRegion),
        "icon" => decode_icon(args).map(Command::Icon),
        "text_run" => decode_text_run(args).map(Command::TextRun),
        "text_run_end" => Some(Command::TextRunEnd),
        "bar" => decode_bar(args).map(Command::Bar),
        "polyline" => decode_polyline(args).map(Command::Polyline),
        "line_layout" => decode_line_layout(args).map(Command::LineLayout),
        "fill" => decode_fill(args).map(Command::Fill),
        "fill_end" => Some(Command::FillEnd),
        "scroll" => decode_scroll(args).map(Command::Scroll),
        "pool_cursor" => decode_pool_cursor(args).map(Command::PoolCursor),
        "reposition" => decode_reposition(args).map(Command::Reposition),
        "pool_drop" => decode_pool_drop(args).map(Command::PoolDrop),
        "minimap" => decode_minimap(args).map(Command::Minimap),
        "minimap_lines" => decode_minimap_lines(args).map(Command::MinimapLines),
        "minimap_view" => decode_minimap_view(args).map(Command::MinimapView),
        "minimap_drop" => decode_minimap_drop(args).map(Command::MinimapDrop),
        "window_open" => decode_window_open(args).map(Command::WindowOpen),
        "window_close" => decode_window_close(args).map(Command::WindowClose),
        "window_focus" => decode_window_focus(args).map(Command::WindowFocus),
        "reset" => Some(Command::Reset),
        "config_reload" => Some(Command::ConfigReload),
        "zoom_capture" => decode_zoom_capture(args),
        "font_step" => decode_font_step(args),
        "hello" => decode_hello(args).map(Command::Hello),
        _ => None,
    }
}

fn decode_border(args: &[Vec<u8>]) -> Option<BorderCommand> {
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

fn decode_panel(args: &[Vec<u8>]) -> Option<PanelCommand> {
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
    })
}

fn decode_scale(args: &[Vec<u8>]) -> Option<ScaleCommand> {
    let arg: &[u8; 5] = args.first()?.get(..5)?.try_into().ok()?;

    Some(ScaleCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        scale: arg[4],
    })
}

/// Decode a `Gstoatty;popover` open marker's head. The `content` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::PopoverEnd`], so it is empty here.
fn decode_popover(args: &[Vec<u8>]) -> Option<PopoverCommand> {
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

fn decode_scroll_region(args: &[Vec<u8>]) -> Option<ScrollRegionCommand> {
    let arg: &[u8; 10] = args.first()?.get(..10)?.try_into().ok()?;

    Some(ScrollRegionCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        offset: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

fn decode_pool_region(args: &[Vec<u8>]) -> Option<PoolRegionCommand> {
    let arg: &[u8; 16] = args.first()?.get(..16)?.try_into().ok()?;

    Some(PoolRegionCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        top: u16::from_be_bytes([arg[4], arg[5]]),
        left: u16::from_be_bytes([arg[6], arg[7]]),
        width: u16::from_be_bytes([arg[8], arg[9]]),
        height: u16::from_be_bytes([arg[10], arg[11]]),
        window: u32::from_be_bytes([arg[12], arg[13], arg[14], arg[15]]),
    })
}

fn decode_window_open(args: &[Vec<u8>]) -> Option<WindowOpenCommand> {
    let [head, title, ..] = args else {
        return None;
    };
    let head: &[u8; 8] = head.get(..8)?.try_into().ok()?;

    Some(WindowOpenCommand {
        window: u32::from_be_bytes([head[0], head[1], head[2], head[3]]),
        cols: u16::from_be_bytes([head[4], head[5]]),
        rows: u16::from_be_bytes([head[6], head[7]]),
        title: String::from_utf8(title.clone()).ok()?,
    })
}

fn decode_window_close(args: &[Vec<u8>]) -> Option<WindowCloseCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;
    Some(WindowCloseCommand {
        window: u32::from_be_bytes(*arg),
    })
}

fn decode_window_focus(args: &[Vec<u8>]) -> Option<WindowFocusCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;
    Some(WindowFocusCommand {
        window: u32::from_be_bytes(*arg),
    })
}

fn decode_icon(args: &[Vec<u8>]) -> Option<IconCommand> {
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

/// Decode a `Gstoatty;text_run` open marker's head. The `text` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::TextRunEnd`], so it is empty here.
fn decode_text_run(args: &[Vec<u8>]) -> Option<TextRunCommand> {
    let arg = args.first()?;
    if arg.len() < 12 {
        return None;
    }

    // A 12-byte head predates the bg-presence byte and always carries a bg. A
    // 13-byte head gates the bg on its trailing presence byte.
    let bg = if arg.len() >= 13 {
        (arg[12] != 0).then_some([arg[9], arg[10], arg[11]])
    } else {
        Some([arg[9], arg[10], arg[11]])
    };

    Some(TextRunCommand {
        col: i16::from_be_bytes([arg[0], arg[1]]),
        row: i16::from_be_bytes([arg[2], arg[3]]),
        scale: u16::from_be_bytes([arg[4], arg[5]]),
        color: [arg[6], arg[7], arg[8]],
        bg,
        text: String::new(),
    })
}

fn decode_bar(args: &[Vec<u8>]) -> Option<BarCommand> {
    let arg: &[u8; 11] = args.first()?.get(..11)?.try_into().ok()?;

    Some(BarCommand {
        x: i16::from_be_bytes([arg[0], arg[1]]),
        y: i16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        color: [arg[8], arg[9], arg[10]],
    })
}

/// Bytes a `polyline` payload spends before its points, holding the width as a
/// big-endian `u16` followed by rgb.
const POLYLINE_HEAD: usize = 5;

fn decode_polyline(args: &[Vec<u8>]) -> Option<PolylineCommand> {
    let arg = args.first()?;
    let tail = arg.get(POLYLINE_HEAD..)?;
    if tail.len() % 4 != 0 {
        return None;
    }

    let points = tail
        .chunks_exact(4)
        .map(|p| {
            [
                i16::from_be_bytes([p[0], p[1]]),
                i16::from_be_bytes([p[2], p[3]]),
            ]
        })
        .collect();
    Some(PolylineCommand {
        points,
        width: u16::from_be_bytes([arg[0], arg[1]]),
        color: [arg[2], arg[3], arg[4]],
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
    let arg: &[u8; 12] = args.first()?.get(..12)?.try_into().ok()?;

    Some(FillCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        index: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

fn decode_scroll(args: &[Vec<u8>]) -> Option<ScrollCommand> {
    let arg: &[u8; 14] = args.first()?.get(..14)?.try_into().ok()?;

    Some(ScrollCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        fraction: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

fn decode_pool_cursor(args: &[Vec<u8>]) -> Option<PoolCursorCommand> {
    let arg: &[u8; 14] = args.first()?.get(..14)?.try_into().ok()?;

    Some(PoolCursorCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        row: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        col: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

fn decode_reposition(args: &[Vec<u8>]) -> Option<RepositionCommand> {
    let arg: &[u8; 12] = args.first()?.get(..12)?.try_into().ok()?;

    Some(RepositionCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

fn decode_pool_drop(args: &[Vec<u8>]) -> Option<PoolDropCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some(PoolDropCommand {
        pool: u32::from_be_bytes(*arg),
    })
}

fn decode_minimap(args: &[Vec<u8>]) -> Option<MinimapCommand> {
    let head: &[u8; 29] = args.first()?.get(..29)?.try_into().ok()?;
    let palette_bytes = args.get(1)?;
    if palette_bytes.len() % 3 != 0 || palette_bytes.len() / 3 > 64 {
        return None;
    }

    let palette = palette_bytes
        .chunks_exact(3)
        .map(|entry| [entry[0], entry[1], entry[2]])
        .collect();

    Some(MinimapCommand {
        top: u16::from_be_bytes([head[0], head[1]]),
        left: u16::from_be_bytes([head[2], head[3]]),
        width: u16::from_be_bytes([head[4], head[5]]),
        height: u16::from_be_bytes([head[6], head[7]]),
        strip_id: u32::from_be_bytes([head[8], head[9], head[10], head[11]]),
        content_id: u32::from_be_bytes([head[12], head[13], head[14], head[15]]),
        lines_per_cell: head[16],
        max_columns: head[17],
        bg: [head[18], head[19], head[20], head[21]],
        thumb: [head[22], head[23], head[24], head[25]],
        thumb_border: [head[26], head[27], head[28]],
        palette,
    })
}

fn decode_minimap_lines(args: &[Vec<u8>]) -> Option<MinimapLinesCommand> {
    let arg = args.first()?;
    let header: &[u8; 16] = arg.get(..16)?.try_into().ok()?;
    let content_id = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let start = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let removed = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let inserted = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);

    // Grown by push rather than pre-allocated, so an inflated `inserted` cannot
    // force a huge allocation before the run bytes prove short.
    let mut lines = Vec::new();
    let mut cursor = 16;
    for _ in 0..inserted {
        let run_count = *arg.get(cursor)? as usize;
        cursor += 1;
        let end = cursor.checked_add(run_count.checked_mul(3)?)?;
        let run_bytes = arg.get(cursor..end)?;
        let runs = run_bytes
            .chunks_exact(3)
            .map(|run| MinimapRun {
                start_col: run[0],
                len: run[1],
                class: run[2],
            })
            .collect();
        lines.push(runs);
        cursor = end;
    }

    if cursor != arg.len() {
        return None;
    }

    Some(MinimapLinesCommand {
        content_id,
        start,
        removed,
        lines,
    })
}

fn decode_minimap_view(args: &[Vec<u8>]) -> Option<MinimapViewCommand> {
    let arg: &[u8; 10] = args.first()?.get(..10)?.try_into().ok()?;

    Some(MinimapViewCommand {
        strip_id: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        top_256: u32::from_be_bytes([arg[4], arg[5], arg[6], arg[7]]),
        visible_lines: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

fn decode_minimap_drop(args: &[Vec<u8>]) -> Option<MinimapDropCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some(MinimapDropCommand {
        content_id: u32::from_be_bytes(*arg),
    })
}

/// An unknown code falls back to `Light` rather than killing the command, so a
/// style added later draws in the plainest form an older terminal has instead of
/// making the whole border vanish.
fn decode_style(code: u8) -> BorderStyle {
    match code {
        1 => BorderStyle::Heavy,
        2 => BorderStyle::Double,
        3 => BorderStyle::Rounded,
        _ => BorderStyle::Light,
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

/// An unknown code falls back to `Info` rather than killing the command, so a
/// kind added later still marks its line, at the mildest severity an older
/// terminal knows, instead of leaving nothing there.
fn decode_icon_kind(code: u8) -> IconKind {
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
    use super::{
        decode, decode_ident_reply, decode_shadow, decode_stream, encode_bar, encode_border,
        encode_config_reload, encode_fill, encode_fill_end, encode_fill_end_into, encode_fill_into,
        encode_font_step, encode_hello, encode_icon, encode_ident_reply, encode_into,
        encode_line_layout, encode_minimap, encode_minimap_drop, encode_minimap_lines,
        encode_minimap_view, encode_panel, encode_polyline, encode_pool_cursor, encode_pool_drop,
        encode_pool_region, encode_popover, encode_popover_end, encode_reposition, encode_reset,
        encode_scale, encode_scroll, encode_scroll_region, encode_text_run, encode_text_run_end,
        encode_window_close, encode_window_focus, encode_window_open, encode_zoom_capture,
        fill_batch_key, BarCommand, BorderCommand, BorderStyle, Command, FillCommand, HelloCommand,
        IconCommand, IconKind, IdentReply, LineLayoutCommand, LineSummary, MinimapCommand,
        MinimapDropCommand, MinimapLinesCommand, MinimapRun, MinimapViewCommand, PanelCommand,
        PanelShadow, PolylineCommand, PoolCursorCommand, PoolDropCommand, PoolRegionCommand,
        PopoverCommand, RepositionCommand, ScaleCommand, ScrollCommand, ScrollRegionCommand,
        TextRunCommand, WindowCloseCommand, WindowFocusCommand, WindowOpenCommand,
    };
    use crate::frame::{self, MAX_APC_PAYLOAD};

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
    fn hello_round_trips() {
        let command = HelloCommand {
            pid: 12345,
            log_id: "20260718-143022-12345".to_string(),
            hostname: "workstation".to_string(),
            version: "0.1.0 (abc)".to_string(),
            protocol: crate::PROTOCOL_VERSION,
        };
        assert_eq!(
            decode(&encode_hello(&command)),
            Some(Command::Hello(command))
        );
    }

    #[test]
    fn ident_reply_round_trips() {
        let reply = IdentReply {
            pid: 4321,
            log_id: "20260718-143000-4321".to_string(),
            hostname: "workstation".to_string(),
            version: "0.2.0".to_string(),
            protocol: crate::PROTOCOL_VERSION,
        };
        let frame = frame::decode(&encode_ident_reply(&reply)).expect("ident frame decodes");
        assert_eq!(decode_ident_reply(&frame), Some(reply));
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

    /// The frame an encoder emits today with bytes appended to its head
    /// argument, the shape a later version's added field arrives in.
    fn with_grown_head(encoded: &[u8]) -> Vec<u8> {
        let mut frame = frame::decode(encoded).expect("the encoder emits a valid frame");
        frame
            .args
            .first_mut()
            .expect("a head argument")
            .extend_from_slice(&[0xab, 0xcd]);
        frame::encode(&frame)
    }

    /// The frame with one argument more than it carries today, the shape a later
    /// version arrives in when it appends a whole field rather than widening the
    /// head.
    fn with_extra_arg(encoded: &[u8]) -> Vec<u8> {
        let mut frame = frame::decode(encoded).expect("the encoder emits a valid frame");
        frame.args.push(b"later".to_vec());
        frame::encode(&frame)
    }

    /// Just the opening frame of a streamed command's encoding, which is the
    /// only part a whole-frame decode can read.
    fn open_marker(encoded: &[u8]) -> Vec<u8> {
        let end = frame::first_frame_end(encoded).expect("the encoding opens with a frame");
        encoded[..end].to_vec()
    }

    /// One command per fixed-head family, enough to encode and grow.
    fn fixed_head_samples() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            (
                "border",
                encode_border(&BorderCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    style: BorderStyle::Heavy,
                    color: [5, 6, 7],
                }),
            ),
            (
                "scale",
                encode_scale(&ScaleCommand {
                    top: 1,
                    left: 2,
                    scale: 3,
                }),
            ),
            (
                "scroll_region",
                encode_scroll_region(&ScrollRegionCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    offset: 5,
                }),
            ),
            (
                "pool_region",
                encode_pool_region(&PoolRegionCommand {
                    pool: 1,
                    top: 2,
                    left: 3,
                    width: 4,
                    height: 5,
                    window: 6,
                }),
            ),
            (
                "window_close",
                encode_window_close(&WindowCloseCommand { window: 1 }),
            ),
            (
                "window_focus",
                encode_window_focus(&WindowFocusCommand { window: 1 }),
            ),
            (
                "bar",
                encode_bar(&BarCommand {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                    color: [5, 6, 7],
                }),
            ),
            ("fill", encode_fill(&FillCommand { pool: 1, index: 2 })),
            (
                "scroll",
                encode_scroll(&ScrollCommand {
                    pool: 1,
                    page: 2,
                    fraction: 3,
                }),
            ),
            (
                "pool_cursor",
                encode_pool_cursor(&PoolCursorCommand {
                    pool: 1,
                    row: 2,
                    col: 3,
                }),
            ),
            (
                "reposition",
                encode_reposition(&RepositionCommand { pool: 1, page: 2 }),
            ),
            ("pool_drop", encode_pool_drop(&PoolDropCommand { pool: 1 })),
            (
                "minimap_view",
                encode_minimap_view(&MinimapViewCommand {
                    strip_id: 1,
                    top_256: 2,
                    visible_lines: 3,
                }),
            ),
            (
                "minimap_drop",
                encode_minimap_drop(&MinimapDropCommand { content_id: 1 }),
            ),
            // A popover's encoding is an open marker, its streamed content, and
            // a close marker, so only the opening frame is a decodable one.
            (
                "popover",
                open_marker(&encode_popover(&PopoverCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    fill: [5, 6, 7],
                    border: [8, 9, 10],
                    content_fg: [11, 12, 13],
                    scale: 1,
                    offset: [0, 0],
                    bold: false,
                    content: String::new(),
                })),
            ),
            (
                "minimap",
                encode_minimap(&MinimapCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    strip_id: 5,
                    content_id: 6,
                    lines_per_cell: 7,
                    max_columns: 8,
                    bg: [9, 10, 11, 12],
                    thumb: [13, 14, 15, 16],
                    thumb_border: [17, 18, 19],
                    palette: vec![[20, 21, 22]],
                }),
            ),
        ]
    }

    /// A field appended to a command's head must not make an older terminal drop
    /// the whole frame. Tolerance is only useful if it ships before the field,
    /// so this pins it now rather than when the first one grows.
    #[test]
    fn fixed_head_decoders_tolerate_a_later_versions_appended_field() {
        for (name, encoded) in fixed_head_samples() {
            let plain = decode(&encoded);
            assert!(plain.is_some(), "{name} decodes as emitted today");
            assert_eq!(
                decode(&with_grown_head(&encoded)),
                plain,
                "{name} reads the same command with bytes appended to its head"
            );
        }
    }

    /// The same guarantee for the commands that count arguments rather than
    /// measure a head, where a later version appends a whole argument.
    #[test]
    fn arg_counted_decoders_tolerate_a_later_versions_extra_argument() {
        let samples = [
            ("zoom_capture", encode_zoom_capture(true)),
            ("font_step", encode_font_step(-2)),
            (
                "window_open",
                encode_window_open(&WindowOpenCommand {
                    window: 1,
                    cols: 2,
                    rows: 3,
                    title: "t".to_owned(),
                }),
            ),
            (
                "hello",
                encode_hello(&HelloCommand {
                    pid: 1,
                    log_id: "l".to_owned(),
                    hostname: "h".to_owned(),
                    version: "v".to_owned(),
                    protocol: crate::PROTOCOL_VERSION,
                }),
            ),
        ];

        for (name, encoded) in samples {
            let plain = decode(&encoded);
            assert!(plain.is_some(), "{name} decodes as emitted today");
            assert_eq!(
                decode(&with_extra_arg(&encoded)),
                plain,
                "{name} reads the same command with an argument appended"
            );
        }
    }

    /// Every emitter built before the version field sends four arguments, and
    /// a terminal that now expects five still has to read them.
    #[test]
    fn a_handshake_without_a_version_argument_reads_as_version_zero() {
        let hello = HelloCommand {
            pid: 7,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        };

        // The frame a build predating the field emits, which is this one minus
        // its appended argument.
        let mut frame = frame::decode(&encode_hello(&hello)).expect("a valid frame");
        assert_eq!(frame.args.len(), 5, "the encoder appends the version");
        frame.args.pop();

        assert!(
            matches!(
                decode(&frame::encode(&frame)),
                Some(Command::Hello(command)) if command.protocol == 0
            ),
            "an older program's hello still decodes, reporting no version"
        );

        let mut reply = frame::decode(&encode_ident_reply(&IdentReply {
            pid: 7,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        }))
        .expect("a valid frame");
        reply.args.pop();

        assert_eq!(
            decode_ident_reply(&reply).map(|reply| reply.protocol),
            Some(0),
            "an older terminal's ident still decodes, reporting no version"
        );
    }

    /// A code added to either enum later must degrade to a member an older
    /// terminal knows rather than taking the whole command down with it.
    #[test]
    fn unknown_enum_codes_fall_back_instead_of_dropping_the_command() {
        assert_eq!(super::decode_style(9), BorderStyle::Light);
        assert_eq!(super::decode_icon_kind(9), IconKind::Info);

        let mut border = encode_border(&BorderCommand {
            top: 1,
            left: 2,
            width: 3,
            height: 4,
            style: BorderStyle::Heavy,
            color: [5, 6, 7],
        });
        // The style byte sits at offset 8 of the head, so rewrite it through the
        // frame rather than guessing where base64 put it.
        let mut frame = frame::decode(&border).expect("a valid frame");
        frame.args[0][8] = 9;
        border = frame::encode(&frame);

        assert!(
            matches!(decode(&border), Some(Command::Border(command)) if command.style == BorderStyle::Light),
            "an unknown style still draws a border"
        );
    }

    /// The ident reply travels terminal-to-program and decodes through its own
    /// entry point, so it needs the same tolerance proven separately.
    #[test]
    fn ident_reply_tolerates_a_later_versions_extra_argument() {
        let reply = IdentReply {
            pid: 1,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        };
        let encoded = encode_ident_reply(&reply);
        let mut frame = frame::decode(&encoded).expect("the encoder emits a valid frame");
        frame.args.push(b"later".to_vec());

        assert_eq!(decode_ident_reply(&frame), Some(reply));
    }

    #[test]
    fn rejects_wrong_length_border_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a border needs.
        assert!(decode(b"Gstoatty;border;YWJj").is_none());
    }

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
        arg.push(super::style_code(BorderStyle::Rounded));
        arg.extend_from_slice(&[200, 40, 90]);
        arg.push(6);
        arg.push(1);
        arg.extend_from_slice(&[20, 22, 30]);
        arg.push(1);
        arg.extend_from_slice(&16u16.to_be_bytes());
        arg.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(arg.len(), 22);

        assert_eq!(
            super::decode_panel(&[arg]),
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
            })
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
        arg.push(super::style_code(BorderStyle::Rounded));
        arg.extend_from_slice(&[200, 40, 90]);
        arg.push(6);
        arg.push(0);
        arg.extend_from_slice(&[0, 0, 0]);
        arg.push(super::shadow_code(PanelShadow::Tucked));
        arg.push(4);
        assert_eq!(arg.len(), 19);

        assert_eq!(
            super::decode_panel(&[arg]),
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
            })
        );
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

    fn stream_border(top: u16) -> BorderCommand {
        BorderCommand {
            top,
            left: 2,
            width: 3,
            height: 4,
            style: BorderStyle::Light,
            color: [1, 2, 3],
        }
    }

    #[test]
    fn decode_stream_reads_frames_in_emission_order() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.extend(encode_reset());
        bytes.extend(encode_border(&stream_border(9)));

        assert_eq!(
            decode_stream(&bytes),
            vec![
                Command::Border(stream_border(1)),
                Command::Reset,
                Command::Border(stream_border(9)),
            ]
        );
    }

    /// The reason the walk exists. A popover's content rides between its open
    /// and close markers rather than inside a frame, so decoding the open frame
    /// alone hands back an empty string.
    #[test]
    fn decode_stream_stitches_popover_content() {
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
            bold: false,
            content: "two\nlines".to_owned(),
        };

        assert_eq!(
            decode_stream(&encode_popover(&command)),
            vec![Command::Popover(command), Command::PopoverEnd]
        );
    }

    #[test]
    fn decode_stream_stitches_text_run_text() {
        let command = TextRunCommand {
            col: -3,
            row: 8,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([4, 5, 6]),
            text: "src/main.rs".to_owned(),
        };

        assert_eq!(
            decode_stream(&encode_text_run(&command)),
            vec![Command::TextRun(command), Command::TextRunEnd]
        );
    }

    #[test]
    fn decode_stream_reads_a_bel_terminated_frame() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.truncate(bytes.len() - 2);
        bytes.push(0x07);
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1)), Command::Reset]
        );
    }

    /// A page's own escape sequences are not frames. Content cut at the first
    /// ESC rather than at the next introducer stops the walk inside the page and
    /// loses every command after it.
    #[test]
    fn decode_stream_steps_over_a_filled_page() {
        let fill = FillCommand { pool: 2, index: 7 };
        let mut bytes = encode_fill(&fill);
        bytes.extend_from_slice(b"\x1b[H\x1b[38;2;1;2;3mpage text\x1b[0m");
        bytes.extend(encode_fill_end());
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Fill(fill), Command::FillEnd, Command::Reset]
        );
    }

    #[test]
    fn decode_stream_skips_what_it_cannot_read() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.extend_from_slice(b"\x1b_Gkitty;border\x1b\\");
        bytes.extend_from_slice(b"\x1b_Gstoatty;not_a_command\x1b\\");
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1)), Command::Reset],
            "a foreign or unknown frame drops out without ending the walk"
        );
    }

    /// A batch cut mid-frame carries no complete command past the cut. A guess
    /// at one reports a command the emitter never finished.
    #[test]
    fn decode_stream_stops_at_an_unterminated_frame() {
        let mut bytes = encode_border(&stream_border(1));
        let truncated = encode_reset();
        bytes.extend_from_slice(&truncated[..truncated.len() - 2]);

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1))]
        );
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
    fn pool_region_round_trips() {
        let command = PoolRegionCommand {
            pool: 4,
            top: 1,
            left: 2,
            width: 76,
            height: 22,
            window: 2,
        };

        assert_eq!(
            decode(&encode_pool_region(&command)),
            Some(Command::PoolRegion(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_region_payload() {
        // The single arg here decodes to 3 bytes, not the 16 a pool region needs.
        assert!(decode(b"Gstoatty;pool_region;YWJj").is_none());
    }

    #[test]
    fn window_open_round_trips() {
        let command = WindowOpenCommand {
            window: 2,
            cols: 80,
            rows: 24,
            title: "src/main.rs".to_string(),
        };

        assert_eq!(
            decode(&encode_window_open(&command)),
            Some(Command::WindowOpen(command))
        );
    }

    #[test]
    fn window_close_round_trips() {
        let command = WindowCloseCommand { window: 3 };

        assert_eq!(
            decode(&encode_window_close(&command)),
            Some(Command::WindowClose(command))
        );
    }

    #[test]
    fn window_focus_round_trips() {
        let command = WindowFocusCommand { window: 5 };

        assert_eq!(
            decode(&encode_window_focus(&command)),
            Some(Command::WindowFocus(command))
        );
    }

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
        arg.push(super::icon_kind_code(IconKind::Warning));
        arg.extend_from_slice(&[255, 200, 0]);
        arg.push(2);
        assert_eq!(arg.len(), 9);

        assert_eq!(
            super::decode_icon(&[arg]),
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

    #[test]
    fn text_run_end_round_trips() {
        // The text_run head and its streamed text round-trip at the terminal
        // layer (the text streams between the open and text_run_end markers, so a
        // single-frame decode cannot recover it); see the tty_term text_run
        // tests. Here we cover the close marker.
        assert_eq!(decode(&encode_text_run_end()), Some(Command::TextRunEnd));
    }

    #[test]
    fn rejects_wrong_length_text_run_payload() {
        // The first arg here decodes to 3 bytes, short of the 12 a text run head needs.
        assert!(decode(b"Gstoatty;text_run;YWJj").is_none());
    }

    #[test]
    fn text_run_decodes_legacy_arg_without_bg_presence() {
        // A 12-byte head predates the bg-presence byte and decodes to an opaque bg.
        let mut arg = Vec::new();
        arg.extend_from_slice(&(-8i16).to_be_bytes());
        arg.extend_from_slice(&48i16.to_be_bytes());
        arg.extend_from_slice(&192u16.to_be_bytes());
        arg.extend_from_slice(&[150, 160, 170]);
        arg.extend_from_slice(&[24, 26, 32]);
        assert_eq!(arg.len(), 12);

        assert_eq!(
            super::decode_text_run(&[arg]),
            Some(TextRunCommand {
                col: -8,
                row: 48,
                scale: 192,
                color: [150, 160, 170],
                bg: Some([24, 26, 32]),
                text: String::new(),
            })
        );
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
    fn polyline_round_trips() {
        let command = PolylineCommand {
            points: vec![[8, 0], [8, 12], [24, 16]],
            width: 6,
            color: [220, 50, 47],
        };

        assert_eq!(
            decode(&encode_polyline(&command)),
            Some(Command::Polyline(command))
        );
    }

    #[test]
    fn a_one_point_polyline_round_trips_as_a_dot() {
        let command = PolylineCommand {
            points: vec![[-4, 40]],
            width: 6,
            color: [1, 2, 3],
        };

        assert_eq!(
            decode(&encode_polyline(&command)),
            Some(Command::Polyline(command))
        );
    }

    #[test]
    fn rejects_a_polyline_payload_with_a_partial_point() {
        // This arg decodes to six bytes, the five-byte head plus one stray,
        // where a point needs four.
        assert!(decode(b"Gstoatty;polyline;YWJjZGVm").is_none());
    }

    #[test]
    fn rejects_a_polyline_payload_shorter_than_its_head() {
        assert!(decode(b"Gstoatty;polyline;YWJj").is_none());
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

    /// Heights a `line_layout` frame holds. The doc names this ceiling, so the
    /// pair of tests below pins it from both sides.
    const LINE_LAYOUT_CAP: usize = 24567;

    /// Points a `polyline` frame holds, pinned the same way.
    const POLYLINE_CAP: usize = 12283;

    #[test]
    fn a_line_layout_at_the_cap_round_trips() {
        let command = LineLayoutCommand {
            heights: vec![1; LINE_LAYOUT_CAP],
        };
        let encoded = encode_line_layout(&command);

        assert!(
            frame::payload_len(encoded.len()) <= MAX_APC_PAYLOAD,
            "the frame at the documented ceiling fits the scanner's budget, got {}",
            frame::payload_len(encoded.len()),
        );
        assert_eq!(decode(&encoded), Some(Command::LineLayout(command)));
    }

    /// One height past the ceiling. The terminal drops an over-cap payload
    /// whole, so without this the layout vanishes with nothing to say why.
    #[test]
    #[should_panic(expected = "overruns the frame cap")]
    fn a_line_layout_past_the_cap_panics_in_debug() {
        encode_line_layout(&LineLayoutCommand {
            heights: vec![1; LINE_LAYOUT_CAP + 1],
        });
    }

    #[test]
    fn a_polyline_at_the_cap_round_trips() {
        let command = PolylineCommand {
            width: 8,
            color: [1, 2, 3],
            points: vec![[4, 5]; POLYLINE_CAP],
        };
        let encoded = encode_polyline(&command);

        assert!(
            frame::payload_len(encoded.len()) <= MAX_APC_PAYLOAD,
            "the frame at the documented ceiling fits the scanner's budget, got {}",
            frame::payload_len(encoded.len()),
        );
        assert_eq!(decode(&encoded), Some(Command::Polyline(command)));
    }

    #[test]
    #[should_panic(expected = "overruns the frame cap")]
    fn a_polyline_past_the_cap_panics_in_debug() {
        encode_polyline(&PolylineCommand {
            width: 8,
            color: [1, 2, 3],
            points: vec![[4, 5]; POLYLINE_CAP + 1],
        });
    }

    #[test]
    fn fill_round_trips() {
        let command = FillCommand {
            pool: 9,
            index: 4_000_000_000,
        };

        assert_eq!(decode(&encode_fill(&command)), Some(Command::Fill(command)));
    }

    #[test]
    fn fill_end_round_trips() {
        assert_eq!(decode(&encode_fill_end()), Some(Command::FillEnd));
    }

    /// The page a batch fills is read from its opening marker alone.
    ///
    /// A sender with a backlog uses this to drop a fill a later one replaces,
    /// so answering for anything it cannot prove is a fill would drop bytes
    /// that were not safe to lose.
    #[test]
    fn a_batch_is_keyed_only_when_it_opens_with_a_fill() {
        let mut fill = Vec::new();
        encode_fill_into(&mut fill, 3, 41);
        // The page's own bytes, which the key must not walk into. The trailing
        // frame is a whole marker of its own and must not be read either.
        fill.extend_from_slice(b"\x1b[1;1Hpainted rows\x1b[0m");
        encode_fill_end_into(&mut fill);

        assert_eq!(fill_batch_key(&fill), Some((3, 41)));
        assert_eq!(
            fill_batch_key(&encode_fill_end()),
            None,
            "a batch that opens with another command names no page",
        );
        assert_eq!(
            fill_batch_key(b"\x1b[1;1Hjust vt bytes"),
            None,
            "and neither does one that opens with no frame at all",
        );

        let mut truncated = Vec::new();
        encode_fill_into(&mut truncated, 3, 41);
        truncated.truncate(truncated.len() - 1);
        assert_eq!(
            fill_batch_key(&truncated),
            None,
            "an unterminated marker is not a page this can name",
        );
    }

    #[test]
    fn rejects_wrong_length_fill_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a fill index needs.
        assert!(decode(b"Gstoatty;fill;YWJj").is_none());
    }

    #[test]
    fn scroll_round_trips() {
        let command = ScrollCommand {
            pool: 3,
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
        // The single arg here decodes to 3 bytes, not the 14 a scroll offset needs.
        assert!(decode(b"Gstoatty;scroll;YWJj").is_none());
    }

    #[test]
    fn pool_cursor_round_trips() {
        let command = PoolCursorCommand {
            pool: 3,
            row: 5_000_000_000,
            col: 40_000,
        };

        assert_eq!(
            decode(&encode_pool_cursor(&command)),
            Some(Command::PoolCursor(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_cursor_payload() {
        // The single arg here decodes to 3 bytes, not the 14 a cursor anchor needs.
        assert!(decode(b"Gstoatty;pool_cursor;YWJj").is_none());
    }

    #[test]
    fn reposition_round_trips() {
        let command = RepositionCommand {
            pool: 2,
            page: 6_000_000_000,
        };

        assert_eq!(
            decode(&encode_reposition(&command)),
            Some(Command::Reposition(command))
        );
    }

    #[test]
    fn rejects_wrong_length_reposition_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a page index needs.
        assert!(decode(b"Gstoatty;reposition;YWJj").is_none());
    }

    #[test]
    fn pool_drop_round_trips() {
        let command = PoolDropCommand { pool: 7 };

        assert_eq!(
            decode(&encode_pool_drop(&command)),
            Some(Command::PoolDrop(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_drop_payload() {
        // The single arg here decodes to 3 bytes, not the 4 a pool id needs.
        assert!(decode(b"Gstoatty;pool_drop;YWJj").is_none());
    }

    /// Assemble a frame for `sub` from raw (pre-base64) argument bytes, for
    /// crafting the malformed payloads the rejection tests probe.
    fn frame_bytes(sub: &str, args: Vec<Vec<u8>>) -> Vec<u8> {
        frame::encode(&frame::Frame {
            sub: sub.to_string(),
            args,
        })
    }

    #[test]
    fn minimap_round_trips() {
        let command = MinimapCommand {
            top: 0,
            left: 72,
            width: 8,
            height: 40,
            strip_id: 5,
            content_id: 9,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [10, 20, 30, 0],
            thumb: [200, 200, 200, 48],
            thumb_border: [255, 255, 255],
            palette: vec![[0, 0, 0], [1, 2, 3]],
        };

        assert_eq!(
            decode(&encode_minimap(&command)),
            Some(Command::Minimap(command))
        );
    }

    #[test]
    fn minimap_accepts_empty_palette() {
        let command = MinimapCommand {
            top: 0,
            left: 0,
            width: 8,
            height: 8,
            strip_id: 1,
            content_id: 1,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [0, 0, 0, 0],
            thumb: [0, 0, 0, 0],
            thumb_border: [0, 0, 0],
            palette: vec![],
        };

        assert_eq!(
            decode(&encode_minimap(&command)),
            Some(Command::Minimap(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_head() {
        // A 28-byte head is one short of the 29 a minimap declaration needs.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 28], vec![0u8; 3]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_oversized_minimap_palette() {
        // 65 rgb entries (195 bytes) exceeds the 64-entry palette cap.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 29], vec![0u8; 195]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_misaligned_minimap_palette() {
        // 4 palette bytes is not a whole number of rgb triples.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 29], vec![0u8; 4]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn minimap_lines_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 9,
            start: 3,
            removed: 2,
            lines: summaries(vec![
                vec![
                    MinimapRun {
                        start_col: 0,
                        len: 4,
                        class: 2,
                    },
                    MinimapRun {
                        start_col: 6,
                        len: 3,
                        class: 5,
                    },
                ],
                vec![MinimapRun {
                    start_col: 2,
                    len: 8,
                    class: 1,
                }],
            ]),
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    /// Split `bytes` into the APC payloads it carries, the stretches between
    /// `ESC _` and the terminator that the terminal's scanner buffers and caps.
    fn apc_payloads(bytes: &[u8]) -> Vec<&[u8]> {
        bytes
            .split(|&b| b == 0x1b)
            .filter_map(|part| part.strip_prefix(b"_"))
            .collect()
    }

    /// Apply one splice to `store`, mirroring the terminal's own store update.
    fn apply_splice(store: &mut Vec<LineSummary>, c: &MinimapLinesCommand) {
        let start = (c.start as usize).min(store.len());
        let end = start.saturating_add(c.removed as usize).min(store.len());
        store.splice(start..end, c.lines.iter().cloned());
    }

    /// The shared summaries a store holds, for stating a fixture.
    fn summaries(lines: Vec<Vec<MinimapRun>>) -> Vec<LineSummary> {
        lines.into_iter().map(Into::into).collect()
    }

    /// A splice of `lines` rows at `runs` runs each, sized by the caller to sit
    /// either side of one APC payload's worth of content.
    fn dense_splice(lines: usize, runs: usize) -> MinimapLinesCommand {
        MinimapLinesCommand {
            content_id: 7,
            start: 2,
            removed: 5,
            lines: (0..lines)
                .map(|i| {
                    (0..runs)
                        .map(|r| MinimapRun {
                            start_col: (r * 2) as u8,
                            len: (i % 7 + 1) as u8,
                            class: (r % 4) as u8,
                        })
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn an_oversized_splice_paginates_within_the_payload_cap() {
        let command = dense_splice(4096, 12);
        let bytes = encode_minimap_lines(&command);
        let payloads = apc_payloads(&bytes);

        assert!(
            payloads.len() > 1,
            "a 4096-line by 12-run splice cannot fit one payload",
        );
        for payload in &payloads {
            assert!(
                payload.len() <= MAX_APC_PAYLOAD,
                "a payload of {} bytes would be discarded by the scanner",
                payload.len(),
            );
        }
    }

    #[test]
    fn paginated_frames_apply_to_the_same_store_as_one_splice() {
        let command = dense_splice(4096, 12);

        let mut expected: Vec<LineSummary> = summaries(vec![vec![]; 40]);
        apply_splice(&mut expected, &command);

        let mut got: Vec<LineSummary> = summaries(vec![vec![]; 40]);
        let bytes = encode_minimap_lines(&command);
        let mut frames = 0;
        for payload in apc_payloads(&bytes) {
            match decode(payload) {
                Some(Command::MinimapLines(part)) => {
                    assert_eq!(part.content_id, command.content_id);
                    apply_splice(&mut got, &part);
                    frames += 1;
                },
                other => panic!("frame {frames} decoded as {other:?}"),
            }
        }

        assert!(frames > 1, "the splice really did paginate");
        assert_eq!(
            got, expected,
            "applying every frame in order rebuilds the single splice's result",
        );
    }

    #[test]
    fn a_splice_that_fits_stays_one_frame() {
        let command = dense_splice(64, 3);
        assert_eq!(
            apc_payloads(&encode_minimap_lines(&command)).len(),
            1,
            "an ordinary splice is not split",
        );
        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command)),
            "and still round-trips as one command",
        );
    }

    #[test]
    fn minimap_lines_pure_deletion_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 4,
            start: 10,
            removed: 3,
            lines: vec![],
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    #[test]
    fn minimap_lines_blank_line_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 4,
            start: 0,
            removed: 0,
            lines: summaries(vec![vec![]]),
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    #[test]
    fn rejects_truncated_minimap_lines_runs() {
        let mut arg = Vec::new();
        arg.extend_from_slice(&9u32.to_be_bytes()); // content_id
        arg.extend_from_slice(&0u32.to_be_bytes()); // start
        arg.extend_from_slice(&0u32.to_be_bytes()); // removed
        arg.extend_from_slice(&1u32.to_be_bytes()); // inserted = 1
        arg.push(2); // the line claims 2 runs, but no run bytes follow

        let bytes = frame_bytes("minimap_lines", vec![arg]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_trailing_bytes_minimap_lines() {
        let mut arg = Vec::new();
        arg.extend_from_slice(&9u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes()); // inserted = 0
        arg.push(0); // a stray byte past the last declared line

        let bytes = frame_bytes("minimap_lines", vec![arg]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn minimap_view_round_trips() {
        let command = MinimapViewCommand {
            strip_id: 5,
            top_256: 1_280,
            visible_lines: 40,
        };

        assert_eq!(
            decode(&encode_minimap_view(&command)),
            Some(Command::MinimapView(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_view_payload() {
        // The single arg here decodes to 3 bytes, not the 10 a view needs.
        assert!(decode(b"Gstoatty;minimap_view;YWJj").is_none());
    }

    #[test]
    fn minimap_drop_round_trips() {
        let command = MinimapDropCommand { content_id: 9 };

        assert_eq!(
            decode(&encode_minimap_drop(&command)),
            Some(Command::MinimapDrop(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_drop_payload() {
        // The single arg here decodes to 3 bytes, not the 4 a content id needs.
        assert!(decode(b"Gstoatty;minimap_drop;YWJj").is_none());
    }

    #[test]
    fn reset_round_trips() {
        assert_eq!(decode(&encode_reset()), Some(Command::Reset));
    }

    #[test]
    fn config_reload_round_trips() {
        assert_eq!(decode(&encode_config_reload()), Some(Command::ConfigReload));
    }

    #[test]
    fn zoom_capture_round_trips_both_states() {
        for on in [true, false] {
            assert_eq!(
                decode(&encode_zoom_capture(on)),
                Some(Command::ZoomCapture { on })
            );
        }
    }

    #[test]
    fn rejects_an_unreadable_zoom_capture_payload() {
        // "eWVz" is base64 for "yes", which is neither of the two words.
        assert!(decode(b"Gstoatty;zoom_capture;eWVz").is_none());
        assert!(
            decode(b"Gstoatty;zoom_capture").is_none(),
            "the claim has to say which way"
        );
    }

    #[test]
    fn font_step_round_trips_in_both_directions() {
        for delta in [1, -1, 4] {
            assert_eq!(
                decode(&encode_font_step(delta)),
                Some(Command::FontStep { delta })
            );
        }
    }

    #[test]
    fn rejects_a_non_numeric_font_step_payload() {
        // "Ymln" is base64 for "big".
        assert!(decode(b"Gstoatty;font_step;Ymln").is_none());
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
            // Popover is a multi-frame open/content/close construct, so it does
            // not round-trip through a single-frame `decode`; its head and
            // streamed content are covered by the tty_term popover tests. Its
            // close marker is single-frame and covered here.
            Command::PopoverEnd,
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
                offset: [0, 0],
            }),
            // TextRun is a multi-frame open/text/close construct, so it does not
            // round-trip through a single-frame `decode`; its head and streamed
            // text are covered by the tty_term text_run tests. Its close marker is
            // single-frame and covered here.
            Command::TextRunEnd,
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
            Command::Fill(FillCommand { pool: 1, index: 7 }),
            Command::FillEnd,
            Command::Scroll(ScrollCommand {
                pool: 2,
                page: 12,
                fraction: 30_000,
            }),
            Command::PoolCursor(PoolCursorCommand {
                pool: 2,
                row: 12,
                col: 30_000,
            }),
            Command::Reposition(RepositionCommand {
                pool: 3,
                page: 1_000,
            }),
            Command::PoolDrop(PoolDropCommand { pool: 4 }),
            Command::Minimap(MinimapCommand {
                top: 1,
                left: 72,
                width: 8,
                height: 40,
                strip_id: 5,
                content_id: 9,
                lines_per_cell: 8,
                max_columns: 120,
                bg: [10, 20, 30, 0],
                thumb: [200, 200, 200, 48],
                thumb_border: [255, 255, 255],
                palette: vec![[0, 0, 0], [1, 2, 3], [4, 5, 6]],
            }),
            Command::MinimapLines(MinimapLinesCommand {
                content_id: 9,
                start: 2,
                removed: 1,
                lines: summaries(vec![
                    vec![MinimapRun {
                        start_col: 0,
                        len: 4,
                        class: 2,
                    }],
                    vec![],
                ]),
            }),
            Command::MinimapView(MinimapViewCommand {
                strip_id: 5,
                top_256: 1_280,
                visible_lines: 40,
            }),
            Command::MinimapDrop(MinimapDropCommand { content_id: 9 }),
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
