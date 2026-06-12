//! Terminal emulator core backed by the `alacritty_terminal` engine.
//!
//! Wraps an [`alacritty_terminal`] `Term` and the `vte` ANSI [`Processor`] into
//! a pure feed/render surface: [`Emulator::feed`] advances the parser over PTY
//! output bytes, and the rendered screen, cursor, mode flags, and queued
//! replies are read back through the accessors. It performs no IO of its own --
//! the caller owns the PTY read loop and writes [`EmulatorEvents::pty_writes`]
//! (device-attribute / status replies) back to the child. This is what lets a
//! query-driven program (DA/DSR) make progress, which the in-house grid could
//! never answer.
//!
//! Single-threaded by construction: the [`EventListener`] queues events into a
//! shared [`Rc`] cell drained after each feed, so the emulator stays on the
//! view's thread and never spawns work.

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{cell::Flags, Config, Term, TermMode},
    vte::ansi::{Color as AnsiColor, CursorShape as AnsiCursorShape, Processor},
};
use std::{cell::RefCell, rc::Rc};

/// A cell color resolved to a neutral representation the view maps to theme
/// colors. [`TermColor::Default`] is the terminal's own default fg/bg sentinel,
/// distinct from any palette entry, so the view can substitute its theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<AnsiColor> for TermColor {
    fn from(color: AnsiColor) -> Self {
        match color {
            AnsiColor::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
            AnsiColor::Indexed(index) => TermColor::Indexed(index),
            // The 256-palette named colors map straight to their index; the
            // foreground/background (and their dim/bright variants, all >= 256)
            // are the "use the view's default" sentinel.
            AnsiColor::Named(named) => match u8::try_from(named as usize) {
                Ok(index) => TermColor::Indexed(index),
                Err(_) => TermColor::Default,
            },
        }
    }
}

/// Cursor rendering shape. [`CursorShape::Hidden`] also encodes an invisible
/// cursor (the program sent `?25l`), so [`Cursor::visible`] keys off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl From<AnsiCursorShape> for CursorShape {
    fn from(shape: AnsiCursorShape) -> Self {
        match shape {
            AnsiCursorShape::Block => CursorShape::Block,
            AnsiCursorShape::Underline => CursorShape::Underline,
            AnsiCursorShape::Beam => CursorShape::Beam,
            AnsiCursorShape::HollowBlock => CursorShape::HollowBlock,
            AnsiCursorShape::Hidden => CursorShape::Hidden,
        }
    }
}

/// One rendered screen cell: its glyph, colors, and style flags.
///
/// A double-width glyph occupies two columns: the first carries the glyph with
/// `wide` set, the second is a `wide_spacer` placeholder the view skips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCell {
    pub c: char,
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub wide: bool,
    pub wide_spacer: bool,
}

/// The cursor's screen position and shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub line: usize,
    pub column: usize,
    pub shape: CursorShape,
}

impl Cursor {
    pub fn visible(&self) -> bool {
        self.shape != CursorShape::Hidden
    }
}

/// Side effects drained from the emulator after a [`Emulator::feed`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmulatorEvents {
    /// Bytes the child requested be written back to it (DA/DSR replies, etc.),
    /// in the order produced. The caller forwards these to the PTY.
    pub pty_writes: Vec<Vec<u8>>,
    /// The most recent window title (OSC 0/2), if one was set. An empty string
    /// is a title reset.
    pub title: Option<String>,
    /// The most recent OSC 52 clipboard store, if any.
    pub clipboard: Option<String>,
}

/// Fixed terminal dimensions fed to `Term::new`/`resize`.
struct EmulatorSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for EmulatorSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Clone, Default)]
struct EventProxy(Rc<RefCell<Vec<Event>>>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

/// A terminal screen driven by feeding it PTY output bytes.
pub struct Emulator {
    term: Term<EventProxy>,
    parser: Processor,
    events: Rc<RefCell<Vec<Event>>>,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        let events = Rc::new(RefCell::new(Vec::new()));
        let size = EmulatorSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(Config::default(), &size, EventProxy(events.clone()));
        Self {
            term,
            parser: Processor::new(),
            events,
        }
    }

    /// Advance the parser over `bytes`, mutating the screen and queuing any
    /// events (replies, title, clipboard) for the next [`Self::drain_events`].
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the screen to `rows` x `cols`, reflowing content.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(EmulatorSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
    }

    /// Take the events queued since the last call: replies to write back to the
    /// PTY, the latest title, and the latest clipboard store.
    pub fn drain_events(&mut self) -> EmulatorEvents {
        let mut out = EmulatorEvents::default();
        for event in self.events.borrow_mut().drain(..) {
            match event {
                Event::PtyWrite(text) => out.pty_writes.push(text.into_bytes()),
                Event::Title(title) => out.title = Some(title),
                Event::ResetTitle => out.title = Some(String::new()),
                Event::ClipboardStore(_, data) => out.clipboard = Some(data),
                _ => {},
            }
        }
        out
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    pub fn columns(&self) -> usize {
        self.term.columns()
    }

    /// The cell at screen `(row, col)`. Out-of-range coordinates are the
    /// caller's responsibility; valid ranges are `0..rows()` x `0..columns()`.
    pub fn cell(&self, row: usize, col: usize) -> RenderCell {
        let cell = &self.term.grid()[Line(row as i32)][Column(col)];
        let flags = cell.flags;
        RenderCell {
            c: cell.c,
            fg: cell.fg.into(),
            bg: cell.bg.into(),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.contains(Flags::UNDERLINE),
            inverse: flags.contains(Flags::INVERSE),
            hidden: flags.contains(Flags::HIDDEN),
            wide: flags.contains(Flags::WIDE_CHAR),
            wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
        }
    }

    pub fn cursor(&self) -> Cursor {
        let cursor = self.term.renderable_content().cursor;
        Cursor {
            line: cursor.point.line.0.max(0) as usize,
            column: cursor.point.column.0,
            shape: cursor.shape.into(),
        }
    }

    /// Whether the program enabled any mouse-reporting protocol.
    pub fn mouse_report(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether mouse reports use the SGR (1006) extended encoding.
    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether application-cursor-keys mode (DECCKM) is active, so arrow keys
    /// encode as SS3 rather than CSI.
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorShape, Emulator};

    fn row_text(emu: &Emulator, row: usize) -> String {
        (0..emu.columns()).map(|col| emu.cell(row, col).c).collect()
    }

    #[test]
    fn csi_g_moves_cursor_to_column() {
        let mut emu = Emulator::new(4, 20);
        emu.feed(b"\x1b[5G");
        assert_eq!(emu.cursor().column, 4, "CSI 5 G is column 5, zero-based 4");
    }

    #[test]
    fn wide_char_occupies_two_columns() {
        let mut emu = Emulator::new(4, 20);
        emu.feed("世".as_bytes());
        let lead = emu.cell(0, 0);
        let spacer = emu.cell(0, 1);
        assert_eq!(lead.c, '世');
        assert!(lead.wide, "lead cell carries the wide glyph");
        assert!(spacer.wide_spacer, "the next column is a spacer");
        assert_eq!(emu.cursor().column, 2, "cursor advances past both columns");
    }

    #[test]
    fn auto_wrap_continues_on_next_row() {
        let mut emu = Emulator::new(4, 3);
        emu.feed(b"abcd");
        assert_eq!(row_text(&emu, 0), "abc");
        assert_eq!(
            emu.cell(1, 0).c,
            'd',
            "the overflow char wraps to the next row"
        );
    }

    #[test]
    fn cursor_hide_sets_hidden_shape() {
        let mut emu = Emulator::new(4, 20);
        assert!(emu.cursor().visible());
        emu.feed(b"\x1b[?25l");
        assert_eq!(emu.cursor().shape, CursorShape::Hidden);
        assert!(!emu.cursor().visible());
    }

    #[test]
    fn device_status_report_queues_a_reply() {
        let mut emu = Emulator::new(4, 20);
        emu.feed(b"\x1b[6n");
        let events = emu.drain_events();
        assert_eq!(
            events.pty_writes,
            vec![b"\x1b[1;1R".to_vec()],
            "DSR 6n reports the 1-based cursor position"
        );
    }
}
