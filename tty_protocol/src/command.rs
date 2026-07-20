//! The typed command surface: a parsed [`Frame`] dispatched by its namespaced
//! sub-command.
//!
//! [`decode`] is the terminal-facing entry point. It returns `None` for any
//! frame the terminal should ignore, whether malformed or carrying a
//! sub-command this build does not recognize, so an unsupported feature
//! degrades to nothing rather than erroring.

use crate::frame::{self, Frame, FrameScratch};

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
pub type LineSummary = Vec<MinimapRun>;

/// Declare a minimap strip and its rendering parameters.
///
/// The payload of [`Command::Minimap`]. The strip occupies a `width` by `height`
/// cell region at (`top`, `left`); [`Self::strip_id`] names this declaration and
/// [`Self::content_id`] the line-summary store it renders (see
/// [`MinimapLinesCommand`]). Each summarized line is drawn `lines_per_cell` to a
/// cell, up to `max_columns` wide. [`Self::bg`] and [`Self::thumb`] are rgba, the
/// thumb being the viewport overlay; [`Self::thumb_border`] is its rgb outline.
/// [`Self::palette`] holds up to 64 rgb entries a run's class indexes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapCommand {
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
    pub palette: Vec<[u8; 3]>,
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
pub fn encode_minimap(command: &MinimapCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap` frame for `command` to `out`.
pub fn encode_minimap_into(out: &mut Vec<u8>, command: &MinimapCommand) {
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
        for entry in &command.palette {
            w.write_all(entry)?;
        }
        Ok(())
    });
    frame::end(out);
}

/// Encode a [`MinimapLinesCommand`] as a full `Gstoatty;minimap_lines` frame.
///
/// The splice header and every inserted line's runs ride in a single argument.
pub fn encode_minimap_lines(command: &MinimapLinesCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_lines_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap_lines` frame for `command` to `out`.
pub fn encode_minimap_lines_into(out: &mut Vec<u8>, command: &MinimapLinesCommand) {
    frame::begin(out, "minimap_lines");
    frame::push_arg(out, |w| {
        w.write_all(&command.content_id.to_be_bytes())?;
        w.write_all(&command.start.to_be_bytes())?;
        w.write_all(&command.removed.to_be_bytes())?;
        w.write_all(&(command.lines.len() as u32).to_be_bytes())?;
        for line in &command.lines {
            w.write_all(&[line.len() as u8])?;
            for run in line {
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

/// Encode a [`HelloCommand`] as a full `Gstoatty;hello` frame for an emitter.
pub fn encode_hello(command: &HelloCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_hello_into(&mut out, command);
    out
}

/// Append a `Gstoatty;hello` frame for `command` to `out`.
///
/// The four fields ride as separate arguments in field order, with `pid` as a
/// decimal string so the frame stays legible in a raw log.
pub fn encode_hello_into(out: &mut Vec<u8>, command: &HelloCommand) {
    frame::begin(out, "hello");
    frame::push_arg(out, |w| write!(w, "{}", command.pid));
    frame::push_arg(out, |w| w.write_all(command.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.version.as_bytes()));
    frame::end(out);
}

fn decode_hello(args: &[Vec<u8>]) -> Option<HelloCommand> {
    let [pid, log_id, hostname, version] = args else {
        return None;
    };
    Some(HelloCommand {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
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
/// [`Command::Hello`], carrying the same four fields.
pub fn encode_ident_reply_into(out: &mut Vec<u8>, reply: &IdentReply) {
    frame::begin(out, "ident");
    frame::push_arg(out, |w| write!(w, "{}", reply.pid));
    frame::push_arg(out, |w| w.write_all(reply.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.version.as_bytes()));
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
    let [pid, log_id, hostname, version] = frame.args.as_slice() else {
        return None;
    };
    Some(IdentReply {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
    })
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
        "hello" => decode_hello(args).map(Command::Hello),
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
        style: decode_style(arg[8])?,
        border: [arg[9], arg[10], arg[11]],
        corner_radius: arg[12],
        fill: (arg[13] != 0).then_some([arg[14], arg[15], arg[16]]),
        shadow: decode_shadow(arg[17]),
        inset_x: arg[18],
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

/// Decode a `Gstoatty;popover` open marker's head. The `content` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::PopoverEnd`], so it is empty here.
fn decode_popover(args: &[Vec<u8>]) -> Option<PopoverCommand> {
    let region: &[u8; 23] = args.first()?.as_slice().try_into().ok()?;

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
    let arg: &[u8; 10] = args.first()?.as_slice().try_into().ok()?;

    Some(ScrollRegionCommand {
        top: u16::from_be_bytes([arg[0], arg[1]]),
        left: u16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        offset: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

fn decode_pool_region(args: &[Vec<u8>]) -> Option<PoolRegionCommand> {
    let arg: &[u8; 16] = args.first()?.as_slice().try_into().ok()?;

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
    let [head, title] = args else {
        return None;
    };
    let head: &[u8; 8] = head.as_slice().try_into().ok()?;

    Some(WindowOpenCommand {
        window: u32::from_be_bytes([head[0], head[1], head[2], head[3]]),
        cols: u16::from_be_bytes([head[4], head[5]]),
        rows: u16::from_be_bytes([head[6], head[7]]),
        title: String::from_utf8(title.clone()).ok()?,
    })
}

fn decode_window_close(args: &[Vec<u8>]) -> Option<WindowCloseCommand> {
    let arg: &[u8; 4] = args.first()?.as_slice().try_into().ok()?;
    Some(WindowCloseCommand {
        window: u32::from_be_bytes(*arg),
    })
}

fn decode_window_focus(args: &[Vec<u8>]) -> Option<WindowFocusCommand> {
    let arg: &[u8; 4] = args.first()?.as_slice().try_into().ok()?;
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
        kind: decode_icon_kind(arg[4])?,
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
    let arg: &[u8; 12] = args.first()?.as_slice().try_into().ok()?;

    Some(FillCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        index: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

fn decode_scroll(args: &[Vec<u8>]) -> Option<ScrollCommand> {
    let arg: &[u8; 14] = args.first()?.as_slice().try_into().ok()?;

    Some(ScrollCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        fraction: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

fn decode_pool_cursor(args: &[Vec<u8>]) -> Option<PoolCursorCommand> {
    let arg: &[u8; 14] = args.first()?.as_slice().try_into().ok()?;

    Some(PoolCursorCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        row: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        col: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

fn decode_reposition(args: &[Vec<u8>]) -> Option<RepositionCommand> {
    let arg: &[u8; 12] = args.first()?.as_slice().try_into().ok()?;

    Some(RepositionCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

fn decode_pool_drop(args: &[Vec<u8>]) -> Option<PoolDropCommand> {
    let arg: &[u8; 4] = args.first()?.as_slice().try_into().ok()?;

    Some(PoolDropCommand {
        pool: u32::from_be_bytes(*arg),
    })
}

fn decode_minimap(args: &[Vec<u8>]) -> Option<MinimapCommand> {
    let head: &[u8; 29] = args.first()?.as_slice().try_into().ok()?;
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
    let arg: &[u8; 10] = args.first()?.as_slice().try_into().ok()?;

    Some(MinimapViewCommand {
        strip_id: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        top_256: u32::from_be_bytes([arg[4], arg[5], arg[6], arg[7]]),
        visible_lines: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

fn decode_minimap_drop(args: &[Vec<u8>]) -> Option<MinimapDropCommand> {
    let arg: &[u8; 4] = args.first()?.as_slice().try_into().ok()?;

    Some(MinimapDropCommand {
        content_id: u32::from_be_bytes(*arg),
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
        decode, decode_ident_reply, decode_shadow, encode_bar, encode_border, encode_fill,
        encode_fill_end, encode_hello, encode_icon, encode_ident_reply, encode_into,
        encode_line_layout, encode_minimap, encode_minimap_drop, encode_minimap_lines,
        encode_minimap_view, encode_panel, encode_pool_cursor, encode_pool_drop,
        encode_pool_region, encode_popover, encode_popover_end, encode_reposition, encode_reset,
        encode_scale, encode_scroll, encode_scroll_region, encode_text_run_end,
        encode_window_close, encode_window_focus, encode_window_open, BarCommand, BorderCommand,
        BorderStyle, Command, FillCommand, HelloCommand, IconCommand, IconKind, IdentReply,
        LineLayoutCommand, MinimapCommand, MinimapDropCommand, MinimapLinesCommand, MinimapRun,
        MinimapViewCommand, PanelCommand, PanelShadow, PoolCursorCommand, PoolDropCommand,
        PoolRegionCommand, PopoverCommand, RepositionCommand, ScaleCommand, ScrollCommand,
        ScrollRegionCommand, TextRunCommand, WindowCloseCommand, WindowFocusCommand,
        WindowOpenCommand,
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
    fn hello_round_trips() {
        let command = HelloCommand {
            pid: 12345,
            log_id: "20260718-143022-12345".to_string(),
            hostname: "workstation".to_string(),
            version: "0.1.0 (abc)".to_string(),
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
        };
        let frame = crate::frame::decode(&encode_ident_reply(&reply)).expect("ident frame decodes");
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
    fn panel_decode_ignores_legacy_title_gap_bytes() {
        // A 22-byte arg carries three trailing bytes past the 19-byte base from
        // an emitter that still wrote the retired title-gap span. The decoder
        // reads the base -- here a zero inset -- and ignores the rest rather than
        // rejecting the frame.
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
            pool: 9,
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
        crate::frame::encode(&crate::frame::Frame {
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
            lines: vec![
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
            ],
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
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
            lines: vec![vec![]],
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
                lines: vec![
                    vec![MinimapRun {
                        start_col: 0,
                        len: 4,
                        class: 2,
                    }],
                    vec![],
                ],
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
