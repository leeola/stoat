//! The bytes-to-grid driver: a VT byte stream parsed onto the cell grid.
//!
//! [`Terminal`] wraps an `alacritty_terminal` terminal and its vte parser.
//! Bytes fed to [`Terminal::advance`] mutate the parsed screen, and
//! [`Terminal::project`] copies that screen onto a [`Grid`]. The copy resolves
//! each cell's terminal-palette color to concrete channels and touches only the
//! lines the terminal reports as damaged.

use crate::grid::{
    Border, BorderStyle, Borders, Cell, Flags, Grid, Overlay, Rgb, Scale, UnderlineStyle,
};
use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        color::Colors,
        Config, RenderableCursor, TermDamage,
    },
    vte::ansi::{Color, CursorShape as TermCursorShape, NamedColor, Processor},
    Term,
};
use std::mem;
use stoatty_protocol::command::{self, BorderCommand, Command, PopoverCommand, ScaleCommand};

const PALETTE_LEN: usize = 256;
const DEFAULT_FG: Rgb = Rgb::new(0xcc, 0xcc, 0xcc);
const DEFAULT_BG: Rgb = Rgb::new(0x00, 0x00, 0x00);

/// A live terminal driven by a VT byte stream.
///
/// Owns the parsed screen (an `alacritty_terminal` terminal) and the vte parser
/// that feeds it. No IO lives here: the app crate owns the PTY and pushes bytes
/// in via [`Self::advance`], then calls [`Self::project`] to refresh the render
/// grid.
///
/// Carries a default 256-color palette so [`Self::project`] can resolve a cell's
/// indexed or named color to concrete channels. A color the program overrode
/// (via OSC) takes precedence over the default.
pub struct Terminal {
    term: Term<VoidListener>,
    parser: Processor,
    palette: [Rgb; PALETTE_LEN],
    apc: ApcScanner,
    /// Border regions set by `Gstoatty;border` frames, stamped onto the grid by
    /// [`Self::project`]. They persist until cleared, since the VT projection
    /// resets each cell's borders every frame.
    borders: Vec<BorderCommand>,
    /// Scale commands set by `Gstoatty;scale` frames, applied to the grid by
    /// [`Self::project`]. Like borders, they persist across the per-frame VT
    /// projection that resets each cell's scale.
    scales: Vec<ScaleCommand>,
    /// Popover regions set by `Gstoatty;popover` frames, applied to the grid's
    /// overlay list by [`Self::project`]. They float above the cells, so they
    /// are grid-level overlays rather than cell attributes.
    popovers: Vec<PopoverCommand>,
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
    /// Create a `rows` by `cols` terminal with an empty screen and default palette.
    pub fn new(rows: usize, cols: usize) -> Terminal {
        let term = Term::new(Config::default(), &GridSize { rows, cols }, VoidListener);

        Terminal {
            term,
            parser: Processor::new(),
            palette: default_palette(),
            apc: ApcScanner::default(),
            borders: Vec::new(),
            scales: Vec::new(),
            popovers: Vec::new(),
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
        }
    }

    /// Resize the terminal to `rows` by `cols`.
    ///
    /// The next [`Self::project`] finds its grid no longer matches and repaints
    /// it wholesale at the new size, so the grid follows without a separate call.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(GridSize { rows, cols });
    }

    /// Copy the parsed screen onto `grid` and return the cursor.
    ///
    /// Only lines the terminal reports as damaged since the previous call are
    /// rewritten, so an unchanged line keeps whatever the prior projection left
    /// in `grid`. When `grid`'s dimensions do not match the terminal it is first
    /// resized, which clears it, and every line is treated as damaged.
    pub fn project(&mut self, grid: &mut Grid) -> Cursor {
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

            *grid.get_mut(row, col) = project_cell(indexed.cell, content.colors, &self.palette);
        }

        let cursor = project_cursor(content.cursor, offset);

        apply_borders(grid, &self.borders);
        apply_scales(grid, &self.scales);
        apply_popovers(grid, &self.popovers);

        self.term.reset_damage();
        cursor
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

fn project_cell(cell: &TermCell, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Cell {
    let fg = resolve(cell.fg, overrides, palette);
    let underline_color = match cell.underline_color() {
        Some(color) => resolve(color, overrides, palette),
        None => fg,
    };

    Cell {
        ch: cell.c,
        fg,
        bg: resolve(cell.bg, overrides, palette),
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
fn resolve(color: Color, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    match color {
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index as usize, overrides, palette),
        Color::Named(named) => named_color(named, overrides, palette),
    }
}

fn named_color(named: NamedColor, overrides: &Colors, palette: &[Rgb; PALETTE_LEN]) -> Rgb {
    if let Some(rgb) = overrides[named as usize] {
        return Rgb::new(rgb.r, rgb.g, rgb.b);
    }

    match named {
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
        ansi if (ansi as usize) < PALETTE_LEN => palette[ansi as usize],
        _ => DEFAULT_FG,
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

/// The 16 standard ANSI colors (xterm defaults), palette indices 0..16.
const ANSI_16: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0xcd, 0x00, 0x00),
    Rgb::new(0x00, 0xcd, 0x00),
    Rgb::new(0xcd, 0xcd, 0x00),
    Rgb::new(0x00, 0x00, 0xee),
    Rgb::new(0xcd, 0x00, 0xcd),
    Rgb::new(0x00, 0xcd, 0xcd),
    Rgb::new(0xe5, 0xe5, 0xe5),
    Rgb::new(0x7f, 0x7f, 0x7f),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x5c, 0x5c, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

/// Build the default 256-color xterm palette.
///
/// Indices 0..16 are the ANSI colors, 16..232 the 6x6x6 color cube, and
/// 232..256 the 24-step grayscale ramp.
fn default_palette() -> [Rgb; PALETTE_LEN] {
    let mut palette = [DEFAULT_BG; PALETTE_LEN];
    palette[..16].copy_from_slice(&ANSI_16);

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
    use crate::grid::{
        Border, BorderStyle, Cell, Flags, Grid, Overlay, Rgb, Scale, UnderlineStyle,
    };
    use stoatty_protocol::command::{
        encode_border, encode_popover, encode_scale, BorderCommand,
        BorderStyle as ProtoBorderStyle, PopoverCommand, ScaleCommand,
    };

    fn project(rows: usize, cols: usize, bytes: &[u8]) -> (Grid, Cursor) {
        let mut terminal = Terminal::new(rows, cols);
        let mut grid = Grid::new(rows, cols);

        terminal.advance(bytes);
        let cursor = terminal.project(&mut grid);

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
    fn project_skips_undamaged_rows() {
        let mut terminal = Terminal::new(3, 4);
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
        let mut terminal = Terminal::new(2, 6);
        let mut grid = Grid::new(1, 1);

        terminal.advance(b"hello");
        terminal.project(&mut grid);

        assert_eq!((grid.rows(), grid.cols()), (2, 6));
        assert_eq!(grid.get(0, 0).ch, 'h');
    }

    #[test]
    fn resize_propagates_to_grid_on_next_project() {
        let mut terminal = Terminal::new(2, 4);
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

        let mut terminal = Terminal::new(2, 3);
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
    fn rounded_border_command_maps_to_rounded_style() {
        let frame = encode_border(&BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: ProtoBorderStyle::Rounded,
            color: [1, 2, 3],
        });

        let mut terminal = Terminal::new(2, 2);
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

        let mut terminal = Terminal::new(2, 2);
        let mut grid = Grid::new(2, 2);
        terminal.advance(&frame);
        terminal.project(&mut grid);

        assert_eq!(grid.get(0, 0).scale, Scale::Origin(2));
        assert_eq!(grid.get(0, 1).scale, Scale::Covered);
        assert_eq!(grid.get(1, 0).scale, Scale::Covered);
        assert_eq!(grid.get(1, 1).scale, Scale::Covered);
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
            content: "ok".to_owned(),
        });

        let mut terminal = Terminal::new(8, 8);
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
                content: "ok".to_owned(),
            }]
        );
    }
}
