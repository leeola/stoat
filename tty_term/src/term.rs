//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

use crate::{
    grid::{
        Bar, Border, BorderStyle, Borders, Cell, Flags, Grid, Icon, IconKind, Overlay, Rgb, Scale,
        ScrollRegion, TextRun, UnderlineStyle,
    },
    theme::Theme,
};
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        Config, RenderableCursor, TermDamage,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor, Processor},
    Term,
};
use std::{cell::RefCell, mem, rc::Rc};
use stoatty_protocol::command::{
    self, BarCommand, BorderCommand, Command, IconCommand, LineLayoutCommand, PopoverCommand,
    ScaleCommand, ScrollRegionCommand, TextRunCommand,
};

const PALETTE_LEN: usize = 256;

/// A live terminal driven by a VT byte stream.
///
/// Owns the parsed screen (an `alacritty_terminal` terminal) and the vte parser
/// that feeds it. No IO lives here: the app crate owns the PTY and pushes bytes
/// in via [`Self::advance`], then calls [`Self::project`] to refresh the render
/// grid.
///
/// Resolves a cell's indexed or named color against its [`Theme`] and the
/// 256-color palette derived from it. A color the program overrode (via OSC)
/// takes precedence over the theme.
pub struct Terminal {
    term: Term<ResponseSink>,
    /// Shares the `term`'s response buffer so [`Self::take_responses`] can drain
    /// the replies the terminal emits to host queries.
    responses: ResponseSink,
    parser: Processor,
    /// Color set the projection resolves named and default colors against.
    theme: Theme,
    palette: [Rgb; PALETTE_LEN],
    apc: ApcScanner,
    /// Border regions set by `Gstoatty;border` frames, stamped onto the grid by
    /// [`Self::project`]. They persist until a `Gstoatty;reset` frame clears
    /// them, since the VT projection resets each cell's borders every frame.
    borders: Vec<BorderCommand>,
    /// Scale commands set by `Gstoatty;scale` frames, applied to the grid by
    /// [`Self::project`]. Like borders, they persist across the per-frame VT
    /// projection that resets each cell's scale.
    scales: Vec<ScaleCommand>,
    /// Popover regions set by `Gstoatty;popover` frames, applied to the grid's
    /// overlay list by [`Self::project`]. They float above the cells, so they
    /// are grid-level overlays rather than cell attributes.
    popovers: Vec<PopoverCommand>,
    /// The scrollable region set by `Gstoatty;scroll_region` frames, applied to
    /// the grid by [`Self::project`]. Unlike the other commands it does not
    /// accumulate: a region's scroll offset updates over time, so the latest
    /// frame replaces the prior one.
    scroll_region: Option<ScrollRegionCommand>,
    /// Status icons set by `Gstoatty;icon` frames, applied to the grid's icon
    /// list by [`Self::project`]. Like popovers they accumulate and are
    /// grid-level rather than cell attributes.
    icons: Vec<IconCommand>,
    /// Text runs set by `Gstoatty;text_run` frames, applied to the grid's
    /// text-run list by [`Self::project`]. Off-grid components, accumulated and
    /// grid-level like the icons.
    text_runs: Vec<TextRunCommand>,
    /// Color bars set by `Gstoatty;bar` frames, applied to the grid's bar list
    /// by [`Self::project`]. Off-grid components, accumulated and grid-level
    /// like the icons.
    bars: Vec<BarCommand>,
    /// The logical-line layout set by `Gstoatty;line_layout` frames, applied to
    /// the grid by [`Self::project`]. Replaced, not accumulated, like the scroll
    /// region: the latest layout wins.
    line_layout: Option<LineLayoutCommand>,
    /// Scrollback line count at the previous [`Self::project`], so the next one
    /// can report how many rows the content scrolled since.
    last_history: usize,
}

/// Where the cursor sits and how it is drawn, as of the last [`Terminal::project`].
///
/// `row` and `col` are zero-based coordinates into the projected [`Grid`]. The
/// grid carries no cursor cell of its own, so the renderer reads this separately
/// to draw the cursor over the cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// The shape a cursor is drawn as.
///
/// A stoatty-owned mirror of the VT cursor styles, so the public API does not
/// leak the `alacritty_terminal` enum. [`CursorShape::Hidden`] means the program
/// asked for the cursor not to be shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl Terminal {
    /// Create a `rows` by `cols` terminal with an empty screen, resolving colors
    /// against `theme`.
    pub fn new(rows: usize, cols: usize, theme: Theme) -> Terminal {
        let responses = ResponseSink::default();
        let term = Term::new(
            Config::default(),
            &GridSize { rows, cols },
            responses.clone(),
        );
        let palette = default_palette(&theme);

        Terminal {
            term,
            responses,
            parser: Processor::new(),
            theme,
            palette,
            apc: ApcScanner::default(),
            borders: Vec::new(),
            scales: Vec::new(),
            popovers: Vec::new(),
            scroll_region: None,
            icons: Vec::new(),
            text_runs: Vec::new(),
            bars: Vec::new(),
            line_layout: None,
            last_history: 0,
        }
    }

    /// Feed `bytes` of the VT stream into the parser, mutating the screen.
    ///
    /// Bytes need not be escape-sequence aligned; the parser retains a partial
    /// sequence across calls.
    ///
    /// Each stoatty `Gstoatty` APC frame in the stream is decoded and applied
    /// before the bytes reach the parser. The bytes are still fed to the parser
    /// verbatim: alacritty consumes the APC string and ignores it, so feeding it
    /// is harmless and avoids the desync that removing bytes would risk.
    pub fn advance(&mut self, bytes: &[u8]) {
        for payload in self.apc.scan(bytes) {
            if let Some(command) = command::decode(&payload) {
                self.apply_command(command);
            }
        }

        self.parser.advance(&mut self.term, bytes);
    }

    /// Take the bytes the terminal wants written back to the PTY, leaving none
    /// buffered.
    ///
    /// Host queries fed to [`Self::advance`] (device attributes, device-status
    /// and cursor-position reports, keyboard-mode queries) produce replies the
    /// shell blocks on; the caller must write them back to the PTY for an
    /// interactive shell to start. Returns empty when the stream held no query.
    pub fn take_responses(&mut self) -> Vec<u8> {
        self.responses.take()
    }

    /// Apply a decoded stoatty command to the terminal.
    ///
    /// The seam every feature sub-code hooks into. A border command is recorded
    /// and stamped onto the grid by [`Self::project`], since it persists across
    /// frames while the VT projection rewrites cells.
    fn apply_command(&mut self, command: Command) {
        match command {
            Command::Border(border) => self.borders.push(border),
            Command::Scale(scale) => self.scales.push(scale),
            Command::Popover(popover) => self.popovers.push(popover),
            Command::ScrollRegion(region) => self.scroll_region = Some(region),
            Command::Icon(icon) => self.icons.push(icon),
            Command::TextRun(text_run) => self.text_runs.push(text_run),
            Command::Bar(bar) => self.bars.push(bar),
            Command::LineLayout(layout) => self.line_layout = Some(layout),
            Command::Reset => self.clear_decorations(),
        }
    }

    /// Clear all accumulated stoatty decoration state.
    ///
    /// A `Gstoatty;reset` frame lands here. Without it the per-frame decoration
    /// lists only grow, since the VT projection re-stamps them every frame, so a
    /// program that redraws a frame at a new position would leave the old one
    /// behind. Resetting lets a program redraw its decoration scene from scratch.
    fn clear_decorations(&mut self) {
        self.borders.clear();
        self.scales.clear();
        self.popovers.clear();
        self.icons.clear();
        self.text_runs.clear();
        self.bars.clear();
        self.scroll_region = None;
        self.line_layout = None;
    }

    /// Resize the terminal to `rows` by `cols`.
    ///
    /// The next [`Self::project`] finds its grid no longer matches and repaints
    /// it wholesale at the new size, so the grid follows without a separate call.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(GridSize { rows, cols });
    }

    /// Copy the parsed screen onto `grid` and return the cursor and the number
    /// of rows the content scrolled since the previous call.
    ///
    /// Only lines the terminal reports as damaged since the previous call are
    /// rewritten, so an unchanged line keeps whatever the prior projection left
    /// in `grid`. When `grid`'s dimensions do not match the terminal it is first
    /// resized, which clears it, and every line is treated as damaged.
    ///
    /// The scroll delta is the growth in scrollback since the previous call: the
    /// rows live output pushed off the top. It is the renderer's signal to ease
    /// vertical scrolling. User scrollback is not counted, and it saturates to
    /// zero once the scrollback history fills.
    pub fn project(&mut self, grid: &mut Grid) -> (Cursor, usize) {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();

        let resized = grid.rows() != rows || grid.cols() != cols;
        if resized {
            grid.resize(rows, cols);
        }

        let dirty = self.collect_damage(rows, resized);

        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 {
                continue;
            }

            let (row, col) = (row as usize, indexed.point.column.0);
            if row >= rows || col >= cols || !dirty.is_dirty(row) {
                continue;
            }

            *grid.get_mut(row, col) =
                project_cell(indexed.cell, content.colors, &self.theme, &self.palette);
        }

        let cursor = project_cursor(content.cursor, offset);

        apply_borders(grid, &self.borders);
        apply_scales(grid, &self.scales);
        apply_popovers(grid, &self.popovers);
        apply_scroll_region(grid, self.scroll_region);
        apply_icons(grid, &self.icons);
        apply_line_layout(grid, self.line_layout.as_ref());
        apply_text_runs(grid, &self.text_runs);
        apply_bars(grid, &self.bars);

        let history = self.term.history_size();
        let scrolled = history.saturating_sub(self.last_history);
        self.last_history = history;

        self.term.reset_damage();
        (cursor, scrolled)
    }

    /// Resolve which rows [`Self::project`] must rewrite this frame.
    ///
    /// `force_full` short-circuits to [`Dirty::Full`] when the grid was just
    /// resized and holds no valid prior content, bypassing the terminal's own
    /// damage which may report only a partial change.
    fn collect_damage(&mut self, rows: usize, force_full: bool) -> Dirty {
        if force_full {
            return Dirty::Full;
        }

        match self.term.damage() {
            TermDamage::Full => Dirty::Full,
            TermDamage::Partial(lines) => {
                let mut rows_dirty = vec![false; rows];
                for bounds in lines {
                    if let Some(slot) = rows_dirty.get_mut(bounds.line) {
                        *slot = true;
                    }
                }
                Dirty::Partial(rows_dirty)
            },
        }
    }
}

/// Captures the bytes the terminal wants written back to the PTY.
///
/// `alacritty_terminal` reports replies to host queries (device attributes,
/// device-status and cursor-position reports, keyboard-mode queries) as
/// [`Event::PtyWrite`] events through its [`EventListener`]. The trait method
/// takes `&self`, so the buffer lives behind an [`Rc`]/[`RefCell`]: the `Term`
/// holds the listener while the owning [`Terminal`] keeps a clone to drain.
/// Other event variants (title, clipboard, bell) are dropped.
#[derive(Clone, Default)]
struct ResponseSink {
    bytes: Rc<RefCell<Vec<u8>>>,
}

impl ResponseSink {
    /// Drain the buffered response bytes, leaving the buffer empty.
    fn take(&self) -> Vec<u8> {
        let mut bytes = self.bytes.borrow_mut();
        mem::take(&mut *bytes)
    }
}

impl EventListener for ResponseSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.bytes.borrow_mut().extend_from_slice(text.as_bytes());
        }
    }
}

const ESC: u8 = 0x1b;
/// Byte after `ESC` that opens an APC string (`ESC _`).
const APC_INTRODUCER: u8 = b'_';
/// Byte after `ESC` that closes a string control (`ESC \`, the ST).
const STRING_TERMINATOR: u8 = b'\\';
/// Bell, accepted as an alternate string terminator.
const BEL: u8 = 0x07;

/// Cap on a buffered APC payload, bounding memory against an APC string that
/// never terminates. Stoatty frames are far smaller, so an overrun is discarded.
const MAX_APC_BYTES: usize = 64 * 1024;

/// Extracts APC string payloads from a VT byte stream as they complete.
///
/// `alacritty_terminal` consumes APC strings without surfacing them, so the
/// driver watches the bytes itself: this tracks the `ESC _ ... ESC \` (or
/// `BEL`) framing across [`Terminal::advance`] calls and yields each completed
/// payload, the bytes between the introducer and the terminator. Recognizing a
/// stoatty frame among the payloads is the decoder's job, not this scanner's.
#[derive(Default)]
struct ApcScanner {
    state: ApcState,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
enum ApcState {
    #[default]
    Ground,
    Escape,
    Apc,
    ApcEscape,
}

impl ApcScanner {
    /// Feed `bytes`, returning every APC payload that completes within them.
    ///
    /// A payload split across calls is retained until its terminator arrives.
    fn scan(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut payloads = Vec::new();

        for &byte in bytes {
            match self.state {
                ApcState::Ground => {
                    if byte == ESC {
                        self.state = ApcState::Escape;
                    }
                },
                ApcState::Escape => {
                    self.state = match byte {
                        APC_INTRODUCER => {
                            self.payload.clear();
                            ApcState::Apc
                        },
                        ESC => ApcState::Escape,
                        _ => ApcState::Ground,
                    };
                },
                ApcState::Apc => match byte {
                    ESC => self.state = ApcState::ApcEscape,
                    BEL => {
                        payloads.push(mem::take(&mut self.payload));
                        self.state = ApcState::Ground;
                    },
                    _ => self.push(byte),
                },
                ApcState::ApcEscape => match byte {
                    STRING_TERMINATOR => {
                        payloads.push(mem::take(&mut self.payload));
                        self.state = ApcState::Ground;
                    },
                    ESC => self.state = ApcState::ApcEscape,
                    _ => {
                        self.payload.clear();
                        self.state = ApcState::Ground;
                    },
                },
            }
        }

        payloads
    }

    /// Buffer one payload byte, abandoning the frame if it overruns the cap.
    fn push(&mut self, byte: u8) {
        if self.payload.len() < MAX_APC_BYTES {
            self.payload.push(byte);
        } else {
            self.payload.clear();
            self.state = ApcState::Ground;
        }
    }
}

/// The set of viewport rows a projection must rewrite.
enum Dirty {
    Full,
    Partial(Vec<bool>),
}

impl Dirty {
    fn is_dirty(&self, row: usize) -> bool {
        match self {
            Dirty::Full => true,
            Dirty::Partial(rows) => rows.get(row).copied().unwrap_or(false),
        }
    }
}

/// Adapts stoatty's row/column count to `alacritty_terminal`'s [`Dimensions`].
///
/// `total_lines` equals `screen_lines`: the terminal grows its own scrollback
/// from the config, so no history rows are declared up front.
struct GridSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

fn project_cell(
    cell: &TermCell,
    overrides: &Colors,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) -> Cell {
    let fg = resolve(cell.fg, overrides, theme, palette);
    let underline_color = match cell.underline_color() {
        Some(color) => resolve(color, overrides, theme, palette),
        None => fg,
    };

    Cell {
        ch: cell.c,
        fg,
        bg: resolve(cell.bg, overrides, theme, palette),
        flags: map_flags(cell.flags),
        underline: map_underline(cell.flags),
        underline_color,
        // Borders and scale come from the stoatty APC, not the VT stream, so a
        // projected cell carries neither.
        borders: Borders::default(),
        scale: Scale::Single,
    }
}

/// Resolve a terminal [`Color`] to concrete channels.
///
/// A program-set `overrides` entry wins over the default palette for the same
/// slot, mirroring how a VT terminal lets OSC redefine palette colors.
fn resolve(color: Color, overrides: &Colors, theme: &Theme, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match color {
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index as usize, overrides, palette),
        Color::Named(named) => named_color(named, overrides, theme, palette),
    }
}

fn named_color(
    named: NamedColor,
    overrides: &Colors,
    theme: &Theme,
    palette: &[Rgb; PALETTE_LEN],
) -> Rgb {
    if let Some(rgb) = overrides[named as usize] {
        return Rgb::new(rgb.r, rgb.g, rgb.b);
    }

    match named {
        NamedColor::Background => theme.background,
        NamedColor::Foreground | NamedColor::BrightForeground => theme.foreground,
        ansi if (ansi as usize) < PALETTE_LEN => palette[ansi as usize],
        _ => theme.foreground,
    }
}

fn indexed(index: usize, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match overrides[index] {
        Some(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        None => palette[index],
    }
}

/// Map the terminal's cell flags to the boolean attributes stoatty's grid
/// carries.
///
/// Underline is not among them; it is mapped separately by [`map_underline`].
/// `INVERSE` and `DIM` stay flags rather than being baked into the colors, so
/// the renderer applies them at draw time.
fn map_flags(flags: TermFlags) -> Flags {
    let mut mapped = Flags::empty();

    if flags.contains(TermFlags::BOLD) {
        mapped |= Flags::BOLD;
    }
    if flags.contains(TermFlags::ITALIC) {
        mapped |= Flags::ITALIC;
    }
    if flags.contains(TermFlags::DIM) {
        mapped |= Flags::DIM;
    }
    if flags.contains(TermFlags::INVERSE) {
        mapped |= Flags::INVERSE;
    }
    if flags.contains(TermFlags::HIDDEN) {
        mapped |= Flags::HIDDEN;
    }
    if flags.contains(TermFlags::STRIKEOUT) {
        mapped |= Flags::STRIKEOUT;
    }

    mapped
}

/// Map the terminal's underline flags to a stoatty [`UnderlineStyle`].
///
/// A cell carries at most one underline flag, so the most specific match wins;
/// a plain `UNDERLINE` is the straight fallback.
fn map_underline(flags: TermFlags) -> UnderlineStyle {
    if flags.contains(TermFlags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(TermFlags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(TermFlags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(TermFlags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if flags.contains(TermFlags::UNDERLINE) {
        UnderlineStyle::Straight
    } else {
        UnderlineStyle::None
    }
}

/// Stamp every stored border region's perimeter edges onto `grid`.
///
/// Runs each projection because the cell projection resets borders to none;
/// edges outside the grid are skipped so a region may extend past it.
fn apply_borders(grid: &mut Grid, commands: &[BorderCommand]) {
    for command in commands {
        frame_region(grid, command);
    }
}

fn frame_region(grid: &mut Grid, command: &BorderCommand) {
    if command.width == 0 || command.height == 0 {
        return;
    }

    let border = Border {
        style: grid_border_style(command.style),
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
    };

    let rows = grid.rows();
    let cols = grid.cols();
    let top = command.top as usize;
    let left = command.left as usize;
    let bottom = top + command.height as usize - 1;
    let right = left + command.width as usize - 1;

    for col in left..=right.min(cols.saturating_sub(1)) {
        if top < rows {
            grid.get_mut(top, col).borders.top = Some(border);
        }
        if bottom < rows {
            grid.get_mut(bottom, col).borders.bottom = Some(border);
        }
    }

    for row in top..=bottom.min(rows.saturating_sub(1)) {
        if left < cols {
            grid.get_mut(row, left).borders.left = Some(border);
        }
        if right < cols {
            grid.get_mut(row, right).borders.right = Some(border);
        }
    }
}

fn grid_border_style(style: command::BorderStyle) -> BorderStyle {
    match style {
        command::BorderStyle::Light => BorderStyle::Light,
        command::BorderStyle::Heavy => BorderStyle::Heavy,
        command::BorderStyle::Double => BorderStyle::Double,
        command::BorderStyle::Rounded => BorderStyle::Rounded,
    }
}

/// Claim each stored scale command's block on `grid`.
///
/// Runs each projection because the cell projection resets every cell to
/// [`Scale::Single`]. An origin outside the grid is skipped, since wire
/// coordinates are untrusted and may point past the screen.
fn apply_scales(grid: &mut Grid, commands: &[ScaleCommand]) {
    for command in commands {
        let (row, col) = (command.top as usize, command.left as usize);
        if row < grid.rows() && col < grid.cols() {
            grid.place_scaled(row, col, command.scale);
        }
    }
}

/// Replace the grid's overlay list with each stored popover command's region.
///
/// Overlays are grid-level rather than per-cell, so the full list is set each
/// projection rather than stamped per cell. The region is clamped or clipped by
/// the renderer, so out-of-grid anchors need no guard here.
fn apply_popovers(grid: &mut Grid, commands: &[PopoverCommand]) {
    let overlays = commands.iter().map(popover_overlay).collect();
    grid.set_overlays(overlays);
}

/// Set the grid's scrollable region from the stored command, or clear it.
///
/// Runs each projection like the other command appliers, since the grid's
/// scroll region is set rather than derived from cells. The renderer clamps or
/// clips an out-of-grid rectangle, so wire coordinates need no guard here.
fn apply_scroll_region(grid: &mut Grid, command: Option<ScrollRegionCommand>) {
    grid.set_scroll_region(command.map(|command| ScrollRegion {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        offset: command.offset,
    }));
}

/// Replace the grid's icon list with each stored icon command's icon.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The renderer clamps an out-of-grid anchor, so wire
/// coordinates need no guard here.
fn apply_icons(grid: &mut Grid, commands: &[IconCommand]) {
    let icons = commands
        .iter()
        .map(|command| Icon {
            top: command.top,
            left: command.left,
            kind: grid_icon_kind(command.kind),
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            size: command.size,
        })
        .collect();
    grid.set_icons(icons);
}

fn grid_icon_kind(kind: command::IconKind) -> IconKind {
    match kind {
        command::IconKind::Error => IconKind::Error,
        command::IconKind::Warning => IconKind::Warning,
        command::IconKind::Info => IconKind::Info,
    }
}

/// Apply the stored logical-line layout to the grid, or clear it when none is
/// set, so [`apply_text_runs`] and [`apply_bars`] can resolve against it.
fn apply_line_layout(grid: &mut Grid, command: Option<&LineLayoutCommand>) {
    grid.set_line_heights(
        command
            .map(|command| command.heights.clone())
            .unwrap_or_default(),
    );
}

/// Replace the grid's text-run list with each stored text-run command's run.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared row is a logical row resolved through the
/// line layout, so a run tracks expansions above it. The renderer clamps an
/// out-of-grid anchor, so wire coordinates need no guard here.
fn apply_text_runs(grid: &mut Grid, commands: &[TextRunCommand]) {
    let text_runs = commands
        .iter()
        .map(|command| TextRun {
            col: command.col,
            row: resolve_logical_row(grid, command.row),
            scale: command.scale,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            bg: Rgb::new(command.bg[0], command.bg[1], command.bg[2]),
            text: command.text.clone(),
        })
        .collect();
    grid.set_text_runs(text_runs);
}

/// Replace the grid's bar list with each stored bar command's rectangle.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared `y` is a logical row resolved through the
/// line layout, so a bar tracks expansions above it.
fn apply_bars(grid: &mut Grid, commands: &[BarCommand]) {
    let bars = commands
        .iter()
        .map(|command| Bar {
            x: command.x,
            y: resolve_logical_row(grid, command.y),
            width: command.width,
            height: command.height,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        })
        .collect();
    grid.set_bars(bars);
}

/// Resolve a component's declared logical row, in sixteenth-cell units, to the
/// physical row it sits on by adding the whole-row expansion above its line.
///
/// A negative row is off the top with no logical line, so it passes through.
fn resolve_logical_row(grid: &Grid, row: i16) -> i16 {
    if row < 0 {
        return row;
    }

    let logical_line = (row / 16) as usize;
    let expansion = grid
        .line_start_row(logical_line)
        .saturating_sub(logical_line);
    let shift = i16::try_from(expansion.saturating_mul(16)).unwrap_or(i16::MAX);
    row.saturating_add(shift)
}

fn popover_overlay(command: &PopoverCommand) -> Overlay {
    Overlay {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        fill: Rgb::new(command.fill[0], command.fill[1], command.fill[2]),
        border: Rgb::new(command.border[0], command.border[1], command.border[2]),
        content_fg: Rgb::new(
            command.content_fg[0],
            command.content_fg[1],
            command.content_fg[2],
        ),
        scale: command.scale,
        offset: command.offset,
        content: command.content.clone(),
    }
}

fn project_cursor(cursor: RenderableCursor, offset: i32) -> Cursor {
    Cursor {
        row: (cursor.point.line.0 + offset).max(0) as usize,
        col: cursor.point.column.0,
        shape: map_shape(cursor.shape),
    }
}

fn map_shape(shape: TermCursorShape) -> CursorShape {
    match shape {
        TermCursorShape::Block => CursorShape::Block,
        TermCursorShape::Underline => CursorShape::Underline,
        TermCursorShape::Beam => CursorShape::Beam,
        TermCursorShape::HollowBlock => CursorShape::HollowBlock,
        TermCursorShape::Hidden => CursorShape::Hidden,
    }
}

/// Build the 256-color palette for `theme`.
///
/// Indices 0..16 are the theme's ANSI colors, 16..232 the 6x6x6 color cube, and
/// 232..256 the 24-step grayscale ramp.
fn default_palette(theme: &Theme) -> [Rgb; PALETTE_LEN] {
    let mut palette = [theme.background; PALETTE_LEN];
    palette[..16].copy_from_slice(&theme.ansi);

    let mut index = 16;
    for r in 0..6u8 {
        for g in 0..6u8 {
            for b in 0..6u8 {
                palette[index] = Rgb::new(cube_channel(r), cube_channel(g), cube_channel(b));
                index += 1;
            }
        }
    }

    for (step, slot) in palette[232..].iter_mut().enumerate() {
        let level = 8 + step as u8 * 10;
        *slot = Rgb::new(level, level, level);
    }

    palette
}

/// Map a 0..6 cube coordinate to its channel value (0, then 95..255 by 40).
fn cube_channel(level: u8) -> u8 {
    if level == 0 {
        0
    } else {
        55 + level * 40
    }
}

#[cfg(test)]
mod tests {
    use super::{ApcScanner, Cursor, CursorShape, Terminal};
    use crate::{
        grid::{
            Bar, Border, BorderStyle, Cell, Flags, Grid, Icon, IconKind, Overlay, Rgb, Scale,
            ScrollRegion, TextRun, UnderlineStyle,
        },
        theme::Theme,
    };
    use stoatty_protocol::command::{
        encode_bar, encode_border, encode_icon, encode_line_layout, encode_popover, encode_reset,
        encode_scale, encode_scroll_region, encode_text_run, BarCommand, BorderCommand,
        BorderStyle as ProtoBorderStyle, IconCommand, IconKind as ProtoIconKind, LineLayoutCommand,
        PopoverCommand, ScaleCommand, ScrollRegionCommand, TextRunCommand,
    };

    fn project(rows: usize, cols: usize, bytes: &[u8]) -> (Grid, Cursor) {
        let mut terminal = Terminal::new(rows, cols, Theme::default());
        let mut grid = Grid::new(rows, cols);

        terminal.advance(bytes);
        let (cursor, _scroll) = terminal.project(&mut grid);

        (grid, cursor)
    }

    #[test]
    fn projects_plain_text() {
        let (grid, cursor) = project(2, 4, b"hi");

        assert_eq!(grid.get(0, 0).ch, 'h');
        assert_eq!(grid.get(0, 1).ch, 'i');
        assert_eq!(*grid.get(0, 2), Cell::default());
        assert_eq!(*grid.get(1, 0), Cell::default());
        assert_eq!(
            cursor,
            Cursor {
                row: 0,
                col: 2,
                shape: CursorShape::Block
            }
        );
    }

    #[test]
    fn projects_sgr_color_and_bold() {
        let (grid, _) = project(1, 3, b"\x1b[1;31mX");
        let cell = grid.get(0, 0);

        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.fg, Rgb::new(0xcd, 0x00, 0x00));
        assert!(cell.flags.contains(Flags::BOLD));
    }

    #[test]
    fn projects_underline_style_and_color() {
        let (grid, _) = project(1, 3, b"\x1b[4:3;58:2::0:255:0mU");
        let cell = grid.get(0, 0);

        assert_eq!(cell.ch, 'U');
        assert_eq!(cell.underline, UnderlineStyle::Curly);
        assert_eq!(cell.underline_color, Rgb::new(0, 255, 0));
    }

    #[test]
    fn projects_background_color() {
        let (grid, _) = project(1, 3, b"\x1b[42mY");

        assert_eq!(grid.get(0, 0).bg, Rgb::new(0x00, 0xcd, 0x00));
    }

    #[test]
    fn projects_indexed_color() {
        let (grid, _) = project(1, 2, b"\x1b[38;5;231mZ");

        assert_eq!(grid.get(0, 0).fg, Rgb::new(0xff, 0xff, 0xff));
    }

    #[test]
    fn project_resolves_colors_against_theme() {
        let theme = Theme {
            foreground: Rgb::new(4, 5, 6),
            background: Rgb::new(1, 2, 3),
            cursor: Rgb::new(7, 8, 9),
            ansi: [Rgb::new(10, 11, 12); 16],
        };
        let mut terminal = Terminal::new(1, 4, theme);
        let mut grid = Grid::new(1, 4);

        terminal.advance(b"a\x1b[31mb");
        terminal.project(&mut grid);

        assert_eq!(
            grid.get(0, 0).fg,
            Rgb::new(4, 5, 6),
            "default fg from theme"
        );
        assert_eq!(
            grid.get(0, 0).bg,
            Rgb::new(1, 2, 3),
            "default bg from theme"
        );
        assert_eq!(
            grid.get(0, 1).fg,
            Rgb::new(10, 11, 12),
            "ANSI red from theme palette"
        );
    }

    #[test]
    fn projects_cursor_position() {
        let (_, cursor) = project(3, 5, b"\x1b[2;3H");

        assert_eq!(
            cursor,
            Cursor {
                row: 1,
                col: 2,
                shape: CursorShape::Block
            }
        );
    }

    #[test]
    fn captures_host_query_responses_for_the_pty() {
        let mut terminal = Terminal::new(4, 8, Theme::default());

        terminal.advance(b"\x1b[6n");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[1;1R".to_vec(),
            "cursor position report"
        );

        terminal.advance(b"\x1b[c");
        assert_eq!(
            terminal.take_responses(),
            b"\x1b[?6c".to_vec(),
            "primary device attributes"
        );

        assert!(
            terminal.take_responses().is_empty(),
            "buffer drained after taking"
        );
    }

    #[test]
    fn project_reports_rows_scrolled() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        // Four lines into a two-row screen push the top two off into history.
        terminal.advance(b"a\r\nb\r\nc\r\nd");
        let (_, scrolled) = terminal.project(&mut grid);
        assert_eq!(scrolled, 2, "rows scrolled into history");

        // A projection with no new output reports no scroll.
        let (_, scrolled) = terminal.project(&mut grid);
        assert_eq!(scrolled, 0, "no further scroll");
    }

    #[test]
    fn project_skips_undamaged_rows() {
        let mut terminal = Terminal::new(3, 4, Theme::default());
        let mut grid = Grid::new(3, 4);

        terminal.advance(b"AB\r\nCD");
        terminal.project(&mut grid);
        assert_eq!(grid.get(1, 1).ch, 'D');

        grid.get_mut(2, 0).ch = 'Z';

        terminal.advance(b"E");
        terminal.project(&mut grid);

        assert_eq!(grid.get(1, 2).ch, 'E');
        assert_eq!(grid.get(2, 0).ch, 'Z');
        assert_eq!(grid.get(0, 0).ch, 'A');
    }

    #[test]
    fn project_resizes_grid_to_terminal() {
        let mut terminal = Terminal::new(2, 6, Theme::default());
        let mut grid = Grid::new(1, 1);

        terminal.advance(b"hello");
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (2, 6));
        assert_eq!(grid.get(0, 0).ch, 'h');
    }

    #[test]
    fn resize_propagates_to_grid_on_next_project() {
        let mut terminal = Terminal::new(2, 4, Theme::default());
        let mut grid = Grid::new(2, 4);

        terminal.advance(b"hi");
        terminal.project(&mut grid);
        assert_eq!((grid.rows(), grid.cols()), (2, 4));

        terminal.resize(5, 10);
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (5, 10));
    }

    #[test]
    fn scans_single_apc_frame() {
        let mut scanner = ApcScanner::default();

        assert_eq!(
            scanner.scan(b"\x1b_Gstoatty;border\x1b\\"),
            vec![b"Gstoatty;border".to_vec()]
        );
    }

    #[test]
    fn scans_frame_split_across_calls() {
        let mut scanner = ApcScanner::default();

        assert!(scanner.scan(b"\x1b_Gstoat").is_empty());
        assert_eq!(scanner.scan(b"ty;x\x1b\\"), vec![b"Gstoatty;x".to_vec()]);
    }

    #[test]
    fn scans_bel_terminated_frame() {
        let mut scanner = ApcScanner::default();

        assert_eq!(scanner.scan(b"\x1b_foo\x07"), vec![b"foo".to_vec()]);
    }

    #[test]
    fn scans_frame_between_text() {
        let mut scanner = ApcScanner::default();

        assert_eq!(scanner.scan(b"a\x1b_foo\x1b\\b"), vec![b"foo".to_vec()]);
    }

    #[test]
    fn scans_two_frames_in_one_chunk() {
        let mut scanner = ApcScanner::default();

        assert_eq!(
            scanner.scan(b"\x1b_a\x1b\\\x1b_b\x1b\\"),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn csi_and_plain_text_yield_no_frames() {
        let mut scanner = ApcScanner::default();

        assert!(scanner.scan(b"hello\x1b[31mworld").is_empty());
    }

    #[test]
    fn apc_frame_is_not_rendered_as_text() {
        let (grid, _) = project(1, 8, b"\x1b_Gstoatty;border\x1b\\hi");

        assert_eq!(grid.get(0, 0).ch, 'h');
        assert_eq!(grid.get(0, 1).ch, 'i');
        assert_eq!(*grid.get(0, 2), Cell::default());
    }

    #[test]
    fn border_apc_frame_frames_the_region() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        let edge = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(255, 0, 0),
        });
        assert_eq!(grid.get(0, 0).borders.top, edge);
        assert_eq!(grid.get(0, 0).borders.left, edge);
        assert_eq!(grid.get(1, 2).borders.bottom, edge);
        assert_eq!(grid.get(1, 2).borders.right, edge);
        assert_eq!(grid.get(1, 1).borders.top, None);
    }

    #[test]
    fn reset_clears_accumulated_borders() {
        let border = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 3,
            height: 2,
            style: ProtoBorderStyle::Light,
            color: [255, 0, 0],
        });

        let mut terminal = Terminal::new(2, 3, Theme::default());
        let mut grid = Grid::new(2, 3);
        terminal.advance(&border);
        terminal.advance(&encode_reset());
        terminal.project(&mut grid);

        assert_eq!(grid.get(0, 0).borders.top, None);
        assert_eq!(grid.get(0, 0).borders.left, None);
    }

    #[test]
    fn rounded_border_command_maps_to_rounded_style() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: ProtoBorderStyle::Rounded,
            color: [1, 2, 3],
        });

        let mut terminal = Terminal::new(2, 2, Theme::default());
        let mut grid = Grid::new(2, 2);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.get(0, 0).borders.top,
            Some(Border {
                style: BorderStyle::Rounded,
                color: Rgb::new(1, 2, 3),
            })
        );
    }

    #[test]
    fn scale_apc_frame_claims_the_block() {
        let frame = encode_scale(&ScaleCommand {
            top: 0,
            left: 0,
            scale: 2,
        });

        let mut terminal = Terminal::new(2, 2, Theme::default());
        let mut grid = Grid::new(2, 2);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(grid.get(0, 0).scale, Scale::Origin(2));
        assert_eq!(grid.get(0, 1).scale, Scale::Covered);
        assert_eq!(grid.get(1, 0).scale, Scale::Covered);
        assert_eq!(grid.get(1, 1).scale, Scale::Covered);
    }

    #[test]
    fn scroll_region_apc_frame_sets_and_replaces_the_region() {
        let region = |offset| ScrollRegion {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            offset,
        };
        let frame = |offset| {
            encode_scroll_region(&ScrollRegionCommand {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                offset,
            })
        };

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);

        terminal.advance(&frame(5));
        terminal.project(&mut grid);
        assert_eq!(grid.scroll_region(), Some(region(5)));

        // A later frame replaces the offset rather than adding a second region.
        terminal.advance(&frame(9));
        terminal.project(&mut grid);
        assert_eq!(grid.scroll_region(), Some(region(9)));
    }

    #[test]
    fn popover_apc_frame_sets_a_grid_overlay() {
        let frame = encode_popover(&PopoverCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [4, -2],
            content: "ok".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.overlays(),
            [Overlay {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                fill: Rgb::new(10, 20, 30),
                border: Rgb::new(40, 50, 60),
                content_fg: Rgb::new(70, 80, 90),
                scale: 2,
                offset: [4, -2],
                content: "ok".to_owned(),
            }]
        );
    }

    #[test]
    fn icon_apc_frame_sets_a_grid_icon() {
        let frame = encode_icon(&IconCommand {
            top: 4,
            left: 1,
            kind: ProtoIconKind::Warning,
            color: [255, 200, 0],
            size: 2,
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.icons(),
            [Icon {
                top: 4,
                left: 1,
                kind: IconKind::Warning,
                color: Rgb::new(255, 200, 0),
                size: 2,
            }]
        );
    }

    #[test]
    fn text_run_apc_frame_sets_a_grid_text_run() {
        let frame = encode_text_run(&TextRunCommand {
            col: -8,
            row: 48,
            scale: 192,
            color: [150, 160, 170],
            bg: [24, 26, 32],
            text: "42".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.text_runs(),
            [TextRun {
                col: -8,
                row: 48,
                scale: 192,
                color: Rgb::new(150, 160, 170),
                bg: Rgb::new(24, 26, 32),
                text: "42".to_owned(),
            }]
        );
    }

    #[test]
    fn bar_apc_frame_sets_a_grid_bar() {
        let frame = encode_bar(&BarCommand {
            x: -4,
            y: 32,
            width: 3,
            height: 16,
            color: [220, 50, 47],
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(
            grid.bars(),
            [Bar {
                x: -4,
                y: 32,
                width: 3,
                height: 16,
                color: Rgb::new(220, 50, 47),
            }]
        );
    }

    #[test]
    fn line_layout_shifts_a_bound_component_past_an_expansion() {
        // Line 1 is three rows tall, so its two extra rows push logical line 3
        // down to physical row 5 (80 sixteenths).
        let layout = encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 3, 1],
        });
        let run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 48,
            scale: 256,
            color: [150, 160, 170],
            bg: [0, 0, 0],
            text: "4".to_owned(),
        });
        let bar = encode_bar(&BarCommand {
            x: 0,
            y: 48,
            width: 2,
            height: 16,
            color: [220, 50, 47],
        });

        let mut terminal = Terminal::new(8, 8, Theme::default());
        let mut grid = Grid::new(8, 8);
        terminal.advance(&layout);
        terminal.advance(&run);
        terminal.advance(&bar);
        terminal.project(&mut grid);

        assert_eq!(grid.text_runs()[0].row, 80, "run shifts down two rows");
        assert_eq!(grid.bars()[0].y, 80, "bar shifts down two rows");
    }
}
