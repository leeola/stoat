use super::{
    cargo::{BuildReport, CmdReport, LineFeed, TestReport},
    kind::{self, RunKind},
    pty::CmdProcess,
};
use crate::{
    location_picker::ParsedLocation,
    osc_cap::{OscCap, MAX_OSC_CLIPBOARD_BYTES, MAX_OSC_PLAIN_BYTES},
    theme::scope,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use ratatui::style::{Color, Modifier};
use smallvec::SmallVec;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::Instant,
};
use unicode_width::UnicodeWidthChar;

#[derive(Clone)]
pub struct StyledCell {
    pub ch: char,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

impl StyledCell {
    /// Whether the cell is the default one.
    ///
    /// A default cell paints nothing, so a row drops the default cells past its
    /// last written one.
    fn is_blank(&self) -> bool {
        self.ch == ' ' && self.fg.is_none() && self.bg.is_none() && self.modifiers.is_empty()
    }
}

impl Default for StyledCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: None,
            bg: None,
            modifiers: Modifier::empty(),
        }
    }
}

/// Rows of output a run block's grid keeps before the oldest scroll off the
/// top, matching what a terminal emulator retains by default.
pub(crate) const MAX_SCROLLBACK_ROWS: usize = 10_000;

/// How far past [`MAX_SCROLLBACK_ROWS`] the grid runs before it is trimmed back
/// to the cap, so the cost of dropping rows is paid once per this many lines
/// rather than once per line.
pub(crate) const SCROLLBACK_SLACK: usize = 512;

pub struct VtermGrid {
    cells: Vec<Vec<StyledCell>>,
    cursor_row: usize,
    cursor_col: usize,
    width: u16,
    pen_fg: Option<Color>,
    pen_bg: Option<Color>,
    pen_modifiers: Modifier,
    pub alt_screen_detected: bool,
    /// Persisted across `feed` calls so escape sequences whose bytes
    /// straddle two PTY reads finish parsing on the second call instead
    /// of being dropped at the chunk boundary.
    parser: vte::Parser,
    /// Keeps an oversized OSC payload out of [`Self::parser`], whose own OSC
    /// buffer has no bound and keeps its capacity for the block's life.
    /// Persisted for the same reason the parser is: an escape that straddles
    /// two reads is counted on its total.
    osc: OscCap,
    /// OSC 52 ("set clipboard") payloads decoded from the input stream.
    /// Callers drain after [`Self::feed`] and forward to a clipboard
    /// host; the grid does not own clipboard side effects.
    pub clipboard_writes: Vec<String>,
    generation: u64,
    /// Rows dropped off the front during the running [`VtermGrid::feed`], which
    /// reports it so a caller can move what it anchored to a row index. Reset
    /// at the start of each feed.
    trimmed_rows: usize,
}

impl VtermGrid {
    pub fn new(width: u16) -> Self {
        Self {
            cells: vec![Vec::with_capacity(width as usize)],
            cursor_row: 0,
            cursor_col: 0,
            width,
            pen_fg: None,
            pen_bg: None,
            pen_modifiers: Modifier::empty(),
            alt_screen_detected: false,
            parser: vte::Parser::new(),
            osc: OscCap::new(MAX_OSC_PLAIN_BYTES, MAX_OSC_CLIPBOARD_BYTES),
            clipboard_writes: Vec::new(),
            generation: 0,
            trimmed_rows: 0,
        }
    }

    /// A grid whose OSC caps are `plain_cap` and `clipboard_cap`, so a test
    /// crosses a cap without a fixture the size of the real one.
    #[cfg(test)]
    pub(super) fn with_osc_caps(width: u16, plain_cap: usize, clipboard_cap: usize) -> Self {
        Self {
            osc: OscCap::new(plain_cap, clipboard_cap),
            ..Self::new(width)
        }
    }

    /// Counter advanced by every [`Self::feed`], the only call that can change
    /// what the grid holds.
    ///
    /// A caller that caches a rendering of this grid compares the counter rather
    /// than re-rendering and diffing. It counts feeds rather than actual cell
    /// changes, so output that lands on identical cells still advances it.
    /// Erring that way is the safe direction. A redundant re-render costs a
    /// frame's work, while a missed one shows stale output.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn line_count(&self) -> usize {
        self.cells.len()
    }

    /// Row count with a single trailing all-default row dropped.
    ///
    /// A command whose output ends in a newline leaves the cursor on a fresh
    /// blank row. Skipping it lets the run pane render output flush like a
    /// terminal instead of with a trailing gap. Both the renderer and
    /// [`crate::run::RunState::active_block_grid_pos`] size the block by this
    /// so the drawn rows and the mouse-hit rows stay in lockstep.
    pub fn rendered_line_count(&self) -> usize {
        match self.cells.len().checked_sub(1) {
            Some(last) if self.row_is_blank(last) => last,
            _ => self.cells.len(),
        }
    }

    fn row_is_blank(&self, idx: usize) -> bool {
        self.cells[idx].iter().all(StyledCell::is_blank)
    }

    /// Drop the cursor row's cells past its last written one, and give back
    /// the width it reserved.
    ///
    /// A line feed and a cursor down call this before they move the cursor
    /// off the row. A finished line then holds its own cells rather than the
    /// pane's width. That width is most of what a flood of short lines into a
    /// wide pane costs.
    ///
    /// A cursor up leaves the row as it is, because a redraw that moves up
    /// comes back to that row.
    fn leave_row(&mut self) {
        let Some(row) = self.cells.get_mut(self.cursor_row) else {
            return;
        };
        let written = row
            .iter()
            .rposition(|cell| !cell.is_blank())
            .map_or(0, |last| last + 1);
        row.truncate(written);
        row.shrink_to_fit();
    }

    pub fn row(&self, idx: usize) -> &[StyledCell] {
        &self.cells[idx]
    }

    /// The cells row `idx` has room for, so a test sees a finished row give
    /// back the width it reserved.
    #[cfg(test)]
    pub(super) fn row_capacity(&self, idx: usize) -> usize {
        self.cells[idx].capacity()
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    /// Extract the row-major text covered by `selection`. Single-row
    /// selections cover columns `[low_col, high_col]` inclusive;
    /// multi-row selections cover `[low_col, end-of-row]` on the first
    /// row, every column on intermediate rows, and `[start, high_col]`
    /// on the last row -- mirroring [`GridSelection::contains`]. Per-row
    /// trailing whitespace is trimmed and rows join with `\n` with no
    /// trailing newline. Out-of-grid selections produce `""`.
    pub fn text_for_selection(&self, selection: &GridSelection) -> String {
        let ((low_col, low_row), (high_col, high_row)) = selection.bounds();
        let width = self.width as usize;
        let low_col = low_col as usize;
        let high_col = high_col as usize;
        let low_row = low_row as usize;
        let high_row = high_row as usize;

        if low_row >= self.cells.len() {
            return String::new();
        }

        if low_row == high_row {
            return self.text_in(low_col..high_col.saturating_add(1), low_row..high_row + 1);
        }

        let mut parts: Vec<String> = Vec::with_capacity(high_row - low_row + 1);
        parts.push(self.text_in(low_col..width, low_row..low_row + 1));
        if high_row > low_row + 1 {
            parts.push(self.text_in(0..width, low_row + 1..high_row));
        }
        parts.push(self.text_in(0..high_col.saturating_add(1), high_row..high_row + 1));
        parts.join("\n")
    }

    /// Extract the characters within a column/row rect as a string. Both
    /// ranges are clamped to grid bounds, trailing whitespace is trimmed
    /// per row, and rows are joined with `\n` with no trailing newline.
    /// Out-of-bounds or inverted ranges produce `""`.
    pub fn text_in(&self, cols: Range<usize>, rows: Range<usize>) -> String {
        let row_start = rows.start.min(self.cells.len());
        let row_end = rows.end.min(self.cells.len());
        if row_start >= row_end {
            return String::new();
        }

        let width = self.width as usize;
        let col_start = cols.start.min(width);
        let col_end = cols.end.min(width);

        let mut lines: Vec<String> = Vec::with_capacity(row_end - row_start);
        for row in &self.cells[row_start..row_end] {
            let slice_end = col_end.min(row.len());
            // The cell after a wide character is its covered half, a blank
            // rather than a character of its own.
            let raw: String = (col_start..slice_end)
                .filter(|&col| col == 0 || UnicodeWidthChar::width(row[col - 1].ch) != Some(2))
                .map(|col| row[col].ch)
                .collect();
            lines.push(raw.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Advance the terminal by `bytes`, reporting how many rows scrolled off
    /// the front to stay inside the scrollback cap.
    ///
    /// A caller holding a row index into this grid moves it down by the count,
    /// or drops it when it addressed a row that is gone.
    pub fn feed(&mut self, bytes: &[u8]) -> usize {
        self.generation += 1;
        self.trimmed_rows = 0;

        let mut spans = SmallVec::<[Range<usize>; 2]>::new();
        let reset = self.osc.spans(bytes, &mut spans);

        let mut parser = if reset {
            vte::Parser::new()
        } else {
            std::mem::take(&mut self.parser)
        };
        for span in spans {
            parser.advance(self, &bytes[span]);
        }
        self.parser = parser;
        self.trimmed_rows
    }

    fn ensure_row(&mut self, row: usize) {
        while self.cells.len() <= row {
            // Only the row the cursor lands on takes writes, so only it
            // reserves the width. A row the cursor jumps past stays empty.
            let reserve = if self.cells.len() == row {
                self.width as usize
            } else {
                0
            };
            self.cells.push(Vec::with_capacity(reserve));
        }
        self.trim_scrollback();
    }

    /// Drop the oldest rows once the grid has run past its cap.
    ///
    /// Waits for [`SCROLLBACK_SLACK`] rows past [`MAX_SCROLLBACK_ROWS`] before
    /// trimming back to the cap. Draining a row for each new one would move the
    /// whole buffer for every line a command prints.
    ///
    /// The cursor moves down with the rows so it still addresses the same row of
    /// output. Every caller re-reads it rather than holding what it passed to
    /// [`Self::ensure_row`], so the adjustment reaches all of them.
    fn trim_scrollback(&mut self) {
        if self.cells.len() <= MAX_SCROLLBACK_ROWS + SCROLLBACK_SLACK {
            return;
        }
        let excess = self.cells.len() - MAX_SCROLLBACK_ROWS;
        self.cells.drain(..excess);
        self.cursor_row = self.cursor_row.saturating_sub(excess);
        self.trimmed_rows += excess;
    }

    /// Write `ch` at the cursor and move the cursor past it.
    ///
    /// A wide character takes two cells, the second a blank that ratatui
    /// covers with the glyph, and a zero-width character keeps one. A
    /// character that does not fit in what is left of the row wraps to the
    /// next one. The wrap waits for that character rather than following the
    /// one that fills the row, so a line that fills its row and then ends with
    /// CRLF stays one row, as in a terminal.
    fn put_char(&mut self, ch: char) {
        let w = usize::from(self.width).max(1);
        let cw = UnicodeWidthChar::width(ch).unwrap_or(1).clamp(1, 2).min(w);
        if self.cursor_col + cw > w {
            self.leave_row();
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
        self.ensure_row(self.cursor_row);

        let row = &mut self.cells[self.cursor_row];
        if row.len() < self.cursor_col + cw {
            row.resize(self.cursor_col + cw, StyledCell::default());
        }
        row[self.cursor_col] = StyledCell {
            ch,
            fg: self.pen_fg,
            bg: self.pen_bg,
            modifiers: self.pen_modifiers,
        };
        if cw == 2 {
            row[self.cursor_col + 1] = StyledCell::default();
        }
        self.cursor_col += cw;
    }

    fn reset_pen(&mut self) {
        self.pen_fg = None;
        self.pen_bg = None;
        self.pen_modifiers = Modifier::empty();
    }
}

impl vte::Perform for VtermGrid {
    fn print(&mut self, c: char) {
        // The parser hands C0 and C1 controls to execute, so DEL is the one
        // control that arrives here. A terminal ignores DEL, and a control
        // character in a cell fails the draw.
        if c.is_control() {
            return;
        }
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.leave_row();
                self.cursor_col = 0;
                self.cursor_row += 1;
                self.ensure_row(self.cursor_row);
            },
            b'\r' => {
                self.cursor_col = 0;
            },
            b'\t' => {
                let next_tab = (self.cursor_col + 8) & !7;
                self.cursor_col = next_tab.min(self.width as usize);
            },
            0x08 => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            },
            _ => {},
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 52 -- "set clipboard". Format: ESC ] 52 ; <Pc> ; <Pd> ST,
        // where <Pc> is the selection ("c" / "p" / "s" / mixed / empty)
        // and <Pd> is base64-encoded text. We honour writes targeted at
        // the system clipboard ("c", empty, or mixed including "c") and
        // ignore primary-only ("p") since the editor has no separate
        // primary-selection plumbing yet.
        if params.len() < 3 || params[0] != b"52" {
            return;
        }
        let selection = params[1];
        let targets_clipboard =
            selection.is_empty() || selection.iter().any(|&b| b == b'c' || b == b's');
        if !targets_clipboard {
            return;
        }
        let bytes = match STANDARD.decode(params[2]) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(
                    target: "stoat::run::vterm",
                    error = %err,
                    "OSC 52 base64 decode failed; dropping payload"
                );
                return;
            },
        };
        match String::from_utf8(bytes) {
            Ok(text) => self.clipboard_writes.push(text),
            Err(err) => {
                tracing::debug!(
                    target: "stoat::run::vterm",
                    error = %err,
                    "OSC 52 payload not UTF-8; dropping"
                );
            },
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let params_vec: Vec<u16> = params.iter().map(|p| p[0]).collect();

        if intermediates == *b"?"
            && action == 'h'
            && (params_vec.contains(&1049) || params_vec.contains(&47))
        {
            self.alt_screen_detected = true;
            return;
        }

        match action {
            'm' => {
                if params_vec.is_empty() {
                    self.reset_pen();
                    return;
                }
                let mut i = 0;
                while i < params_vec.len() {
                    match params_vec[i] {
                        0 => self.reset_pen(),
                        1 => self.pen_modifiers |= Modifier::BOLD,
                        2 => self.pen_modifiers |= Modifier::DIM,
                        3 => self.pen_modifiers |= Modifier::ITALIC,
                        4 => self.pen_modifiers |= Modifier::UNDERLINED,
                        7 => self.pen_modifiers |= Modifier::REVERSED,
                        9 => self.pen_modifiers |= Modifier::CROSSED_OUT,
                        22 => {
                            self.pen_modifiers -= Modifier::BOLD;
                            self.pen_modifiers -= Modifier::DIM;
                        },
                        23 => self.pen_modifiers -= Modifier::ITALIC,
                        24 => self.pen_modifiers -= Modifier::UNDERLINED,
                        27 => self.pen_modifiers -= Modifier::REVERSED,
                        29 => self.pen_modifiers -= Modifier::CROSSED_OUT,
                        30 => self.pen_fg = Some(Color::Black),
                        31 => self.pen_fg = Some(Color::Red),
                        32 => self.pen_fg = Some(Color::Green),
                        33 => self.pen_fg = Some(Color::Yellow),
                        34 => self.pen_fg = Some(Color::Blue),
                        35 => self.pen_fg = Some(Color::Magenta),
                        36 => self.pen_fg = Some(Color::Cyan),
                        37 => self.pen_fg = Some(Color::White),
                        38 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_fg = Some(Color::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        38 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_fg = Some(Color::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        39 => self.pen_fg = None,
                        40 => self.pen_bg = Some(Color::Black),
                        41 => self.pen_bg = Some(Color::Red),
                        42 => self.pen_bg = Some(Color::Green),
                        43 => self.pen_bg = Some(Color::Yellow),
                        44 => self.pen_bg = Some(Color::Blue),
                        45 => self.pen_bg = Some(Color::Magenta),
                        46 => self.pen_bg = Some(Color::Cyan),
                        47 => self.pen_bg = Some(Color::White),
                        48 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_bg = Some(Color::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        48 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_bg = Some(Color::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        49 => self.pen_bg = None,
                        90 => self.pen_fg = Some(Color::DarkGray),
                        91 => self.pen_fg = Some(Color::LightRed),
                        92 => self.pen_fg = Some(Color::LightGreen),
                        93 => self.pen_fg = Some(Color::LightYellow),
                        94 => self.pen_fg = Some(Color::LightBlue),
                        95 => self.pen_fg = Some(Color::LightMagenta),
                        96 => self.pen_fg = Some(Color::LightCyan),
                        97 => self.pen_fg = Some(Color::White),
                        100 => self.pen_bg = Some(Color::DarkGray),
                        101 => self.pen_bg = Some(Color::LightRed),
                        102 => self.pen_bg = Some(Color::LightGreen),
                        103 => self.pen_bg = Some(Color::LightYellow),
                        104 => self.pen_bg = Some(Color::LightBlue),
                        105 => self.pen_bg = Some(Color::LightMagenta),
                        106 => self.pen_bg = Some(Color::LightCyan),
                        107 => self.pen_bg = Some(Color::White),
                        _ => {},
                    }
                    i += 1;
                }
            },
            'A' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            },
            'B' => {
                let n = first_param(&params_vec, 1) as usize;
                self.leave_row();
                self.cursor_row += n;
                self.ensure_row(self.cursor_row);
            },
            'C' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.width as usize - 1);
            },
            'D' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            },
            'K' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                let row = &mut self.cells[self.cursor_row];
                // A row holds only the cells written to it. An erase to the end
                // drops those cells, and the prefix erase of mode 1 stops at
                // the row's end.
                match mode {
                    0 => row.truncate(self.cursor_col),
                    1 => {
                        for cell in row.iter_mut().take(self.cursor_col.min(w - 1) + 1) {
                            *cell = StyledCell::default();
                        }
                    },
                    2 => row.clear(),
                    _ => {},
                }
            },
            'J' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                let (cursor_row, cursor_col) = (self.cursor_row, self.cursor_col);
                match mode {
                    0 => {
                        self.cells[cursor_row].truncate(cursor_col);
                        for row in &mut self.cells[cursor_row + 1..] {
                            row.clear();
                        }
                    },
                    1 => {
                        for row in &mut self.cells[..cursor_row] {
                            row.clear();
                        }
                        for cell in self.cells[cursor_row]
                            .iter_mut()
                            .take(cursor_col.min(w - 1) + 1)
                        {
                            *cell = StyledCell::default();
                        }
                    },
                    2 => {
                        for row in &mut self.cells {
                            row.clear();
                        }
                    },
                    _ => {},
                }
            },
            _ => {},
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

fn first_param(params: &[u16], default: u16) -> u16 {
    params
        .first()
        .copied()
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

/// A two-anchor selection over a [`VtermGrid`]. Coordinates are
/// `(col, row)` cell positions in the grid's coordinate space, not
/// terminal-relative -- consumers translate from screen coords to grid
/// coords before constructing the selection. `anchor` is the click
/// position; `head` follows the drag. [`Self::bounds`] normalizes the
/// pair for row-major iteration regardless of drag direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GridSelection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl GridSelection {
    /// The selection after `rows` have scrolled off the front of the grid, or
    /// `None` when all of it went with them.
    ///
    /// An endpoint that fell off clamps to row 0 rather than dropping the whole
    /// selection, so one that straddled the trim keeps covering what is left of
    /// what the user marked.
    pub fn shifted_up(self, rows: usize) -> Option<Self> {
        let rows = u16::try_from(rows).unwrap_or(u16::MAX);
        if self.anchor.1 < rows && self.head.1 < rows {
            return None;
        }
        Some(Self {
            anchor: (self.anchor.0, self.anchor.1.saturating_sub(rows)),
            head: (self.head.0, self.head.1.saturating_sub(rows)),
        })
    }

    /// Returns `(low, high)` in row-major order: lower-row first, and
    /// within the same row, lower-column first. Independent of the
    /// drag direction the selection was constructed from.
    pub fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        let (a_col, a_row) = self.anchor;
        let (h_col, h_row) = self.head;
        if (a_row, a_col) <= (h_row, h_col) {
            ((a_col, a_row), (h_col, h_row))
        } else {
            ((h_col, h_row), (a_col, a_row))
        }
    }

    /// Reports whether `(col, row)` falls inside the selection in
    /// row-major scan order. Single-row selections cover columns
    /// `[low_col, high_col]` inclusive; multi-row selections cover
    /// `[low_col, end-of-row]` on the first row, every column on
    /// intermediate rows, and `[start-of-row, high_col]` on the last
    /// row. Endpoints are inclusive on both sides.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        let ((low_col, low_row), (high_col, high_row)) = self.bounds();
        if row < low_row || row > high_row {
            return false;
        }
        if low_row == high_row {
            return col >= low_col && col <= high_col;
        }
        if row == low_row {
            col >= low_col
        } else if row == high_row {
            col <= high_col
        } else {
            true
        }
    }

    /// Whether the head sits on the anchor cell, as it does after a click that
    /// no drag extended.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// One command line and its run.
///
/// A Run Block is a Cmd after Enter.
pub struct Cmd {
    pub command: String,
    /// The kind of run that the line asks for, read from the line when the Cmd
    /// is made.
    pub(crate) kind: RunKind,
    /// The directory the command runs in, the workspace root when it was
    /// submitted, which the block's prompt line names.
    pub cwd: PathBuf,
    pub grid: VtermGrid,
    pub finished: bool,
    pub exit_status: Option<i32>,
    pub error: Option<String>,
    /// Active selection over [`Self::grid`], which the run-pane renderer paints
    /// in reverse video. `None` means no selection.
    ///
    /// A drag sets it, and the release that copies its text clears it. A plain
    /// click's release clears it too, so only a drag over blank cells, which
    /// copies nothing, stays highlighted after release.
    pub selection: Option<GridSelection>,
    /// The process of the current run, `None` before the first run and after
    /// the run ends.
    pub(crate) process: Option<CmdProcess>,
    /// When the current run started, `None` before the first run.
    pub(crate) started: Option<Instant>,
    /// When the current run ended, `None` while it runs and before the first
    /// run.
    pub(crate) ended: Option<Instant>,
    /// How many runs started, so a notification of an earlier run reads
    /// apart from the current one.
    pub(crate) run_count: u32,
    /// The whole seconds the running process has run, as the last tick wrote
    /// them.
    ///
    /// The header paints this count rather than the clock, so a frame moves
    /// only when a tick writes a new one.
    pub(crate) elapsed_secs: u64,
    /// The counts that the current run's output reports, for a kind whose
    /// output the block reads, as its lines arrive.
    pub(crate) report: Option<CmdReport>,
    /// The bytes of the output line that has not ended, which `report` reads
    /// once the line ends.
    lines: LineFeed,
    /// Whether a block that can show its report shows its output instead, as
    /// the `t` key flips it.
    pub(crate) raw_view: bool,
}

impl Cmd {
    pub fn new(command: String, cwd: PathBuf, width: u16) -> Self {
        let kind = kind::run_kind(&command);
        Self {
            command,
            kind,
            cwd,
            grid: VtermGrid::new(width),
            finished: false,
            exit_status: None,
            error: None,
            selection: None,
            process: None,
            started: None,
            ended: None,
            run_count: 0,
            elapsed_secs: 0,
            report: CmdReport::for_kind(kind),
            lines: LineFeed::default(),
            raw_view: false,
        }
    }

    /// Advance the block's terminal by `bytes`, moving any selection down with
    /// the rows that scrolled off the front.
    ///
    /// A selection whose rows all fell off is dropped, since it no longer marks
    /// anything the grid still holds. A Cmd that keeps a report reads each
    /// line that `bytes` end into it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let trimmed = self.grid.feed(bytes);
        if trimmed > 0 {
            self.selection = self.selection.and_then(|sel| sel.shifted_up(trimmed));
        }
        if let Some(report) = &mut self.report {
            self.lines.push(bytes, &mut |line| report.push_line(line));
        }
    }

    /// Start the report of a new run over from no output, so the counts of the
    /// run before it never reach it.
    pub(crate) fn reset_report(&mut self) {
        self.report = CmdReport::for_kind(self.kind);
        self.lines = LineFeed::default();
    }

    /// The report of a finished run that the block can show in place of its
    /// output.
    ///
    /// A test run's report shows once a test binary's output arrived. A
    /// build's shows when it holds an error or a warning, so a clean build
    /// keeps cargo's own lines.
    pub(crate) fn finished_report(&self) -> Option<&CmdReport> {
        let report = self.report.as_ref().filter(|_| self.finished)?;
        let shows = match report {
            CmdReport::Test(report) => report.binaries > 0,
            CmdReport::Build(report) => !report.errors.is_empty() || !report.warnings.is_empty(),
        };
        shows.then_some(report)
    }

    /// The rows that the block shows in place of its output, `None` for a
    /// block that shows its output.
    ///
    /// A block with a [`Self::finished_report`] shows its report unless
    /// [`Self::raw_view`] is on, as [`test_rows`] and [`build_rows`] lay it
    /// out. The last row counts the output's rows and names `t`, the key that
    /// shows them.
    pub(crate) fn report_rows(&self) -> Option<Vec<BodyRow>> {
        let mut rows = match self.finished_report().filter(|_| !self.raw_view)? {
            CmdReport::Test(report) => test_rows(report),
            CmdReport::Build(report) => build_rows(report),
        };
        let lines = counted(self.grid.rendered_line_count(), "line", "lines");
        rows.push(BodyRow {
            spans: vec![(
                format!("\u{22ef} {lines} of output \u{b7} t terminal view"),
                Tone::Muted,
            )],
        });
        Some(rows)
    }

    /// Rows this block occupies in the run pane, counting its prompt line, its
    /// body rows, and an optional error line.
    ///
    /// The body is the rows of its report when it shows one, as
    /// [`Self::report_rows`] gives them, and otherwise its grid rows with a
    /// trailing all-default row dropped.
    pub fn rendered_line_span(&self) -> usize {
        let body = self
            .report_rows()
            .map_or_else(|| self.grid.rendered_line_count(), |rows| rows.len());
        1 + body + usize::from(self.error.is_some())
    }
}

/// One row that a Run Block draws as spans of text, such as a row of its
/// report or the hint under the newest block.
pub(crate) struct BodyRow {
    /// The row's text in order, each span in the color of its tone.
    pub(crate) spans: Vec<(String, Tone)>,
}

impl BodyRow {
    /// The row's text, its spans joined.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.spans.iter().map(|(text, _)| text.as_str()).collect()
    }
}

/// The color of a span of a [`BodyRow`], as the theme scope it reads.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Tone {
    /// `ui.text.dim`, the color of output the program left uncolored.
    Dim,
    /// `ui.text.muted`, for a location and a note.
    Muted,
    /// `ui.badge.complete`, for a count of passes.
    Success,
    /// `ui.error`, for a failure.
    Danger,
    /// `ui.diagnostic.warning`, for a warning.
    Warning,
    /// `ui.key_label`, for a key that a hint names.
    Key,
}

impl Tone {
    /// The scope of the theme whose foreground the span draws in.
    pub(crate) fn scope(self) -> &'static str {
        match self {
            Tone::Dim => scope::UI_TEXT_DIM,
            Tone::Muted => scope::UI_TEXT_MUTED,
            Tone::Success => scope::UI_BADGE_COMPLETE,
            Tone::Danger => scope::UI_ERROR,
            Tone::Warning => scope::UI_DIAGNOSTIC_WARNING,
            Tone::Key => scope::UI_KEY_LABEL,
        }
    }
}

/// The rows of a test run's report, before the row that counts its output.
///
/// The first row reads the passed count, the failed count when it is over 0,
/// and the test and binary counts. Each failure takes a row with its name
/// and, when its panic named one, the file name and line, and a row under it
/// with its message when it has one.
fn test_rows(report: &TestReport) -> Vec<BodyRow> {
    let (passed, failed, _) = report.totals();
    let mut summary = vec![(format!("{passed} passed"), Tone::Success)];
    if failed > 0 {
        summary.push((format!("  {failed} failed"), Tone::Danger));
    }
    summary.push((
        format!(
            "  {} \u{b7} {}",
            counted(report.total as usize, "test", "tests"),
            counted(report.binaries as usize, "binary", "binaries")
        ),
        Tone::Dim,
    ));
    let mut rows = vec![BodyRow { spans: summary }];

    for failure in &report.failures {
        let mut spans = vec![(format!("\u{2717} {}", failure.name), Tone::Danger)];
        spans.extend(failure.location.as_ref().map(location_span));
        rows.push(BodyRow { spans });
        if let Some(message) = &failure.message {
            rows.push(BodyRow {
                spans: vec![(format!("  {message}"), Tone::Dim)],
            });
        }
    }
    rows
}

/// The rows of a build's report, before the row that counts its output.
///
/// The first row reads the error count and the warning count, each left out
/// at 0. Each error takes a row with U+2717, its message, and its file name
/// and line, and each warning after them a row with U+25B2 in place of
/// U+2717.
fn build_rows(report: &BuildReport) -> Vec<BodyRow> {
    let mut summary = Vec::new();
    if !report.errors.is_empty() {
        summary.push((
            counted(report.errors.len(), "error", "errors"),
            Tone::Danger,
        ));
    }
    if !report.warnings.is_empty() {
        let gap = if summary.is_empty() { "" } else { "  " };
        let warnings = counted(report.warnings.len(), "warning", "warnings");
        summary.push((format!("{gap}{warnings}"), Tone::Warning));
    }
    let mut rows = vec![BodyRow { spans: summary }];

    let errors = report
        .errors
        .iter()
        .map(|diagnostic| (diagnostic, "\u{2717}", Tone::Danger));
    let warnings = report
        .warnings
        .iter()
        .map(|diagnostic| (diagnostic, "\u{25b2}", Tone::Warning));
    for (diagnostic, mark, tone) in errors.chain(warnings) {
        rows.push(BodyRow {
            spans: vec![
                (format!("{mark} {}", diagnostic.message), tone),
                location_span(&diagnostic.location),
            ],
        });
    }
    rows
}

/// The span that names `location`'s file name and line, three spaces after
/// the text before it.
fn location_span(location: &ParsedLocation) -> (String, Tone) {
    let path = Path::new(&location.path);
    let file = path.file_name().unwrap_or(path.as_os_str());
    (
        format!("   {}:{}", file.to_string_lossy(), location.line),
        Tone::Muted,
    )
}

/// `count` and its noun, `one` for a count of 1 and `many` for any other.
pub(crate) fn counted(count: usize, one: &str, many: &str) -> String {
    match count {
        1 => format!("1 {one}"),
        _ => format!("{count} {many}"),
    }
}
