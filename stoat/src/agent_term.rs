//! A fixed-viewport screen emulator for an owned agent (Claude) session.
//!
//! An agent like Claude is a redraw-heavy full-screen TUI. It drives
//! absolute cursor moves, screen erases, scroll regions, and the alternate
//! screen. The Run pane's [`VtermGrid`](crate::run::vterm::VtermGrid) handles
//! only an append-only command-output stream and cannot model that, so an
//! agent session parses its PTY output onto an `alacritty_terminal::Term`
//! instead -- the project's terminal-state layer, which implements the full
//! VT screen.
//!
//! No IO lives here. The event loop owns the PTY and pushes bytes in via
//! [`AgentTerm::feed`]. The renderer reads the screen back through
//! [`AgentTerm::rows`], [`AgentTerm::row`], and [`AgentTerm::cursor`].

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line},
    term::{
        cell::{Cell as TermCell, Flags as TermFlags},
        Config,
    },
    vte::ansi::{Color as AnsiColor, CursorShape as TermCursorShape, NamedColor, Processor},
    Term,
};
use ratatui::style::{Color, Modifier};

/// One projected grid cell, carrying a character and the ratatui style the
/// renderer paints it with.
///
/// `fg`/`bg` are `None` when the cell uses the terminal's default color, so the
/// renderer supplies the pane's own default rather than a baked-in one. Mirrors
/// the Run pane's cell shape so an agent pane renders through the same path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StyledCell {
    pub ch: char,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

impl Default for StyledCell {
    fn default() -> Self {
        StyledCell {
            ch: ' ',
            fg: None,
            bg: None,
            modifiers: Modifier::empty(),
        }
    }
}

/// Zero-based location of the cursor cell within the viewport.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CursorPos {
    pub row: usize,
    pub col: usize,
}

/// A live agent screen driven by a VT byte stream.
///
/// Owns the parsed screen (an `alacritty_terminal::Term`) and the vte parser
/// that feeds it. The viewport is fixed to the size passed to [`Self::new`];
/// the agent redraws within it rather than the screen growing.
///
/// The terminal's replies to host queries are discarded: a read-only emulator
/// has nowhere to send them, so it uses a [`VoidListener`]. Wiring the replies
/// back to the PTY for an interactive session is a separate concern.
pub struct AgentTerm {
    term: Term<VoidListener>,
    parser: Processor,
}

impl AgentTerm {
    /// Create a `rows` by `cols` emulator with a blank screen.
    ///
    /// Both dimensions are clamped to at least one, since a zero-sized grid has
    /// no valid screen.
    pub fn new(rows: u16, cols: u16) -> AgentTerm {
        let dimensions = GridSize {
            rows: (rows as usize).max(1),
            cols: (cols as usize).max(1),
        };

        AgentTerm {
            term: Term::new(Config::default(), &dimensions, VoidListener),
            parser: Processor::new(),
        }
    }

    /// Feed `bytes` of the VT stream into the parser, mutating the screen.
    ///
    /// Bytes need not be escape-sequence aligned. The parser retains a partial
    /// sequence across calls, so a sequence split over two PTY reads finishes
    /// parsing on the second.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// The viewport height in rows.
    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    /// The viewport width in columns.
    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    /// The styled cells of viewport row `idx`, left to right.
    ///
    /// Always returns [`Self::cols`] cells. A row index past the viewport
    /// yields a blank row rather than panicking.
    pub fn row(&self, idx: usize) -> Vec<StyledCell> {
        let cols = self.cols();
        if idx >= self.rows() {
            return vec![StyledCell::default(); cols];
        }

        let grid = self.term.grid();
        let line = &grid[Line(idx as i32)];
        (0..cols)
            .map(|col| convert_cell(&line[Column(col)]))
            .collect()
    }

    /// The cursor cell, or `None` when the agent has hidden it.
    pub fn cursor(&self) -> Option<CursorPos> {
        let content = self.term.renderable_content();
        if matches!(content.cursor.shape, TermCursorShape::Hidden) {
            return None;
        }

        let row = content.cursor.point.line.0 + content.display_offset as i32;
        if row < 0 {
            return None;
        }

        Some(CursorPos {
            row: row as usize,
            col: content.cursor.point.column.0,
        })
    }
}

/// Adapts stoat's row/column count to `alacritty_terminal`'s [`Dimensions`].
///
/// `total_lines` equals `screen_lines`, since the terminal grows its own
/// scrollback from the config and no history rows are declared up front.
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

fn convert_cell(cell: &TermCell) -> StyledCell {
    StyledCell {
        ch: cell.c,
        fg: map_color(cell.fg),
        bg: map_color(cell.bg),
        modifiers: map_modifiers(cell.flags),
    }
}

/// Map a terminal color to a ratatui color, or `None` for the terminal default.
fn map_color(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Indexed(index) => Some(Color::Indexed(index)),
        AnsiColor::Named(named) => map_named(named),
    }
}

/// Map a named ANSI color to its ratatui equivalent.
///
/// The default foreground/background and the non-palette names (cursor, dim,
/// bright-default) resolve to `None` so the renderer applies the pane default.
fn map_named(named: NamedColor) -> Option<Color> {
    let color = match named {
        NamedColor::Black => Color::Black,
        NamedColor::Red => Color::Red,
        NamedColor::Green => Color::Green,
        NamedColor::Yellow => Color::Yellow,
        NamedColor::Blue => Color::Blue,
        NamedColor::Magenta => Color::Magenta,
        NamedColor::Cyan => Color::Cyan,
        NamedColor::White => Color::White,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
        _ => return None,
    };
    Some(color)
}

/// Map terminal cell flags to ratatui modifiers.
///
/// ratatui carries a single underline modifier, so every underline style
/// (straight, double, curly, dotted, dashed) collapses to [`Modifier::UNDERLINED`].
fn map_modifiers(flags: TermFlags) -> Modifier {
    let mut modifiers = Modifier::empty();

    if flags.contains(TermFlags::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if flags.contains(TermFlags::DIM) {
        modifiers |= Modifier::DIM;
    }
    if flags.contains(TermFlags::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if flags.contains(TermFlags::INVERSE) {
        modifiers |= Modifier::REVERSED;
    }
    if flags.contains(TermFlags::HIDDEN) {
        modifiers |= Modifier::HIDDEN;
    }
    if flags.contains(TermFlags::STRIKEOUT) {
        modifiers |= Modifier::CROSSED_OUT;
    }
    if flags.intersects(
        TermFlags::UNDERLINE
            | TermFlags::DOUBLE_UNDERLINE
            | TermFlags::UNDERCURL
            | TermFlags::DOTTED_UNDERLINE
            | TermFlags::DASHED_UNDERLINE,
    ) {
        modifiers |= Modifier::UNDERLINED;
    }

    modifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_row(term: &AgentTerm, idx: usize) -> String {
        term.row(idx)
            .iter()
            .map(|cell| cell.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn default_grid_is_blank() {
        let term = AgentTerm::new(24, 80);
        assert_eq!((term.rows(), term.cols()), (24, 80));
        assert_eq!(text_row(&term, 0), "");
        assert_eq!(term.cursor(), Some(CursorPos { row: 0, col: 0 }));
    }

    #[test]
    fn writes_ascii_and_advances_cursor() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"hello");
        assert_eq!(text_row(&term, 0), "hello");
        assert_eq!(term.cursor(), Some(CursorPos { row: 0, col: 5 }));
    }

    #[test]
    fn newline_advances_row() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"ab\r\ncd");
        assert_eq!(text_row(&term, 0), "ab");
        assert_eq!(text_row(&term, 1), "cd");
    }

    #[test]
    fn ansi_color_applies_and_resets() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"\x1b[31mR\x1b[0mN");
        let row = term.row(0);
        assert_eq!((row[0].ch, row[0].fg), ('R', Some(Color::Red)));
        assert_eq!((row[1].ch, row[1].fg), ('N', None));
    }

    #[test]
    fn escape_sequence_spans_feed_calls() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"\x1b[31");
        term.feed(b"mR");
        let row = term.row(0);
        assert_eq!((row[0].ch, row[0].fg), ('R', Some(Color::Red)));
    }

    #[test]
    fn cursor_position_is_absolute() {
        let mut term = AgentTerm::new(10, 20);
        term.feed(b"\x1b[3;5H");
        assert_eq!(term.cursor(), Some(CursorPos { row: 2, col: 4 }));
    }

    #[test]
    fn erase_display_clears_screen() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"hello\r\nworld");
        term.feed(b"\x1b[2J");
        assert_eq!(text_row(&term, 0), "");
        assert_eq!(text_row(&term, 1), "");
    }

    #[test]
    fn alt_screen_saves_and_restores_primary() {
        let mut term = AgentTerm::new(4, 10);
        term.feed(b"main");
        term.feed(b"\x1b[?1049h\x1b[Halt");
        assert_eq!(text_row(&term, 0), "alt");
        term.feed(b"\x1b[?1049l");
        assert_eq!(text_row(&term, 0), "main");
    }
}
