use base64::{engine::general_purpose::STANDARD, Engine};
use regex::Regex;
use std::{
    collections::VecDeque,
    ops::{BitOrAssign, Range, SubAssign},
};

/// Default scrollback cap (rows retained beyond the visible output).
/// Matches the common terminal default; [`VtermGrid::new`] uses it.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// Upper bound a caller may request via [`VtermGrid::new_with_scrollback`].
const MAX_SCROLLBACK: usize = 100_000;

/// SGR foreground / background color stored per [`StyledCell`].
/// Variants mirror the standard ANSI SGR color set (8 base colors,
/// 8 bright variants, 256-color indexed, and 24-bit RGB) so the SGR
/// parser in [`VtermGrid`] can map raw codes 1-to-1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermColor {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// SGR text-attribute bit-flag set. Each `BOLD` / `DIM` / ... constant
/// is a single bit; flags compose with `|=` and clear with `-=` to
/// mirror the SGR parser's incremental state updates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermModifier(u16);

impl TermModifier {
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const UNDERLINED: Self = Self(1 << 3);
    pub const REVERSED: Self = Self(1 << 4);
    pub const CROSSED_OUT: Self = Self(1 << 5);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn bits(self) -> u16 {
        self.0
    }
}

impl BitOrAssign for TermModifier {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl SubAssign for TermModifier {
    fn sub_assign(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

#[derive(Clone)]
pub struct StyledCell {
    pub ch: char,
    pub fg: Option<TermColor>,
    pub bg: Option<TermColor>,
    pub modifiers: TermModifier,
}

impl Default for StyledCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: None,
            bg: None,
            modifiers: TermModifier::empty(),
        }
    }
}

/// The main screen and cursor stashed while an alternate screen is
/// active, restored when the program leaves it.
struct SavedScreen {
    cells: VecDeque<Vec<StyledCell>>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Mouse-tracking mode a program enabled, selecting which events the run
/// pane forwards as mouse reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No tracking; the pane does local selection.
    None,
    /// `?1000`: press and release only.
    Press,
    /// `?1002`: press, release, and drag (motion with a button held).
    ButtonEvent,
    /// `?1003`: the above plus button-less motion.
    AnyEvent,
}

/// Cursor shape selected via DECSCUSR (`CSI Ps SP q`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// Full-cell block (DECSCUSR 0/1/2). The default.
    Block,
    /// Underline at the cell's baseline (DECSCUSR 3/4).
    Underline,
    /// Vertical bar at the cell's left edge (DECSCUSR 5/6).
    Bar,
}

pub struct VtermGrid {
    cells: VecDeque<Vec<StyledCell>>,
    cursor_row: usize,
    cursor_col: usize,
    width: u16,
    /// Most rows retained. Once `cells` exceeds this the oldest rows are
    /// evicted from the front, bounding memory for long-running output.
    scrollback_limit: usize,
    pen_fg: Option<TermColor>,
    pen_bg: Option<TermColor>,
    pen_modifiers: TermModifier,
    /// The main screen, saved while an alternate screen is active
    /// (`?1049h`/`?47h`). `Some` iff currently on the alt screen;
    /// restored on `?1049l`/`?47l`.
    saved_screen: Option<SavedScreen>,
    /// Top/bottom margins (0-based, inclusive) of the DECSTBM scroll
    /// region. Line feeds at the bottom margin scroll only within these
    /// rows; `None` scrolls the whole buffer.
    scroll_region: Option<(usize, usize)>,
    /// Cursor position stashed by DECSC/SCOSC (`s`), restored by
    /// DECRC/SCORC (`u`).
    saved_cursor: Option<(usize, usize)>,
    /// Mouse-tracking mode set via `?1000`/`?1002`/`?1003`.
    mouse_protocol: MouseProtocol,
    /// Whether mouse reports use SGR encoding (`?1006`) rather than X10.
    mouse_sgr: bool,
    /// Cursor shape selected via DECSCUSR.
    cursor_shape: CursorShape,
    /// Persisted across `feed` calls so escape sequences whose bytes
    /// straddle two PTY reads finish parsing on the second call instead
    /// of being dropped at the chunk boundary.
    parser: vte::Parser,
    /// OSC 52 ("set clipboard") payloads decoded from the input stream.
    /// Callers drain after [`Self::feed`] and forward to a clipboard
    /// host; the grid does not own clipboard side effects.
    pub clipboard_writes: Vec<String>,
    /// Working directory most recently reported via OSC 7, overwritten on
    /// each report. Exposed through [`Self::cwd`].
    cwd: Option<String>,
    /// OSC 133 command boundaries decoded from the input stream. Callers
    /// drain after [`Self::feed`] to bound command blocks and capture
    /// exit status; the grid does not own block lifecycle.
    pub command_marks: Vec<CommandMark>,
}

/// Shell-integration command boundary reported via OSC 133. `Start` is
/// the `C` mark (command output begins); `Done` is the `D;<exit>` mark
/// (command finished), carrying the exit code when the shell supplies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMark {
    Start,
    Done { exit: Option<i32> },
}

impl VtermGrid {
    pub fn new(width: u16) -> Self {
        Self::new_with_scrollback(width, DEFAULT_SCROLLBACK)
    }

    /// As [`Self::new`] but with an explicit scrollback cap, clamped to
    /// `[1, MAX_SCROLLBACK]`.
    pub fn new_with_scrollback(width: u16, scrollback_limit: usize) -> Self {
        Self {
            cells: Self::blank_screen(width),
            cursor_row: 0,
            cursor_col: 0,
            width,
            scrollback_limit: scrollback_limit.clamp(1, MAX_SCROLLBACK),
            pen_fg: None,
            pen_bg: None,
            pen_modifiers: TermModifier::empty(),
            saved_screen: None,
            scroll_region: None,
            saved_cursor: None,
            mouse_protocol: MouseProtocol::None,
            mouse_sgr: false,
            cursor_shape: CursorShape::Block,
            parser: vte::Parser::new(),
            clipboard_writes: Vec::new(),
            cwd: None,
            command_marks: Vec::new(),
        }
    }

    /// A single blank row at the given width -- the starting state of a
    /// screen and what the alt screen swaps in.
    fn blank_screen(width: u16) -> VecDeque<Vec<StyledCell>> {
        let mut cells = VecDeque::new();
        cells.push_back(vec![StyledCell::default(); width as usize]);
        cells
    }

    /// Enter the alternate screen: stash the main screen and cursor, swap
    /// in a blank screen, and home the cursor. A no-op if already on the
    /// alt screen.
    fn enter_alt_screen(&mut self) {
        if self.saved_screen.is_some() {
            return;
        }
        let cells = std::mem::replace(&mut self.cells, Self::blank_screen(self.width));
        self.saved_screen = Some(SavedScreen {
            cells,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        });
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Leave the alternate screen, restoring the saved main screen and
    /// cursor. A no-op if not on the alt screen.
    fn leave_alt_screen(&mut self) {
        if let Some(saved) = self.saved_screen.take() {
            self.cells = saved.cells;
            self.cursor_row = saved.cursor_row;
            self.cursor_col = saved.cursor_col;
        }
    }

    /// Apply the DEC private modes in `params` (`?...h` sets, `?...l`
    /// resets). Unknown modes are ignored.
    fn set_private_modes(&mut self, params: &[u16], set: bool) {
        let mouse = |proto| if set { proto } else { MouseProtocol::None };
        for &param in params {
            match param {
                1049 | 47 => {
                    if set {
                        self.enter_alt_screen();
                    } else {
                        self.leave_alt_screen();
                    }
                },
                1000 => self.mouse_protocol = mouse(MouseProtocol::Press),
                1002 => self.mouse_protocol = mouse(MouseProtocol::ButtonEvent),
                1003 => self.mouse_protocol = mouse(MouseProtocol::AnyEvent),
                1006 => self.mouse_sgr = set,
                _ => {},
            }
        }
    }

    /// The active mouse-tracking mode.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.mouse_protocol
    }

    /// The cursor shape selected via DECSCUSR.
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    /// The cursor's current `(row, col)` cell position.
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Find every match of the regex `query` in the grid, one inclusive
    /// single-row [`GridSelection`] per match. An invalid regex yields no
    /// matches. Matches do not span rows.
    pub fn search(&self, query: &str) -> Vec<GridSelection> {
        let Ok(re) = Regex::new(query) else {
            return Vec::new();
        };
        let mut matches = Vec::new();
        for (row_idx, cells) in self.cells.iter().enumerate() {
            let Ok(row) = u16::try_from(row_idx) else {
                break;
            };
            let text: String = cells.iter().map(|c| c.ch).collect();
            for m in re.find_iter(&text) {
                let start = text[..m.start()].chars().count();
                let end = text[..m.end()].chars().count();
                if end == start {
                    continue;
                }
                matches.push(GridSelection {
                    anchor: (start as u16, row),
                    head: ((end - 1) as u16, row),
                });
            }
        }
        matches
    }

    /// Detect clickable links in the grid: `http`/`https` URLs and
    /// `path:line:col` references whose path part looks like a path
    /// (contains `/` or `.`). Each is one inclusive single-row link.
    pub fn links(&self) -> Vec<TerminalLink> {
        let url_re = Regex::new(r"https?://[^\s]+").expect("valid url regex");
        let path_re = Regex::new(r"([^\s:]+):(\d+)(?::(\d+))?").expect("valid path regex");
        let mut links = Vec::new();
        for (row_idx, cells) in self.cells.iter().enumerate() {
            let Ok(row) = u16::try_from(row_idx) else {
                break;
            };
            let text: String = cells.iter().map(|c| c.ch).collect();
            for m in url_re.find_iter(&text) {
                links.push(TerminalLink {
                    selection: selection_for(&text, m.start(), m.end(), row),
                    target: LinkTarget::Url(m.as_str().to_string()),
                });
            }
            for cap in path_re.captures_iter(&text) {
                let whole = cap.get(0).expect("whole match present");
                let path = cap.get(1).expect("path group present").as_str();
                if !path.contains('/') && !path.contains('.') {
                    continue;
                }
                let line = cap.get(2).and_then(|m| m.as_str().parse().ok());
                let column = cap.get(3).and_then(|m| m.as_str().parse().ok());
                links.push(TerminalLink {
                    selection: selection_for(&text, whole.start(), whole.end(), row),
                    target: LinkTarget::Path {
                        path: path.to_string(),
                        line,
                        column,
                    },
                });
            }
        }
        links
    }

    /// Encode a mouse event as report bytes in the grid's current
    /// encoding (`?1006` SGR or X10), or `None` if the position is
    /// outside what X10 can express. `button` is the base button code
    /// plus the motion (32) and scroll (64) bits; `mods` adds shift (4),
    /// alt (8), and control (16).
    pub fn encode_mouse(
        &self,
        button: u8,
        mods: u8,
        col: u16,
        row: u16,
        pressed: bool,
    ) -> Option<Vec<u8>> {
        encode_mouse_report(self.mouse_sgr, button, mods, col, row, pressed)
    }

    pub fn line_count(&self) -> usize {
        self.cells.len()
    }

    pub fn row(&self, idx: usize) -> &[StyledCell] {
        &self.cells[idx]
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    /// The working directory most recently reported via OSC 7, or `None`
    /// when the program has not reported one.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Resize the grid to `width` columns, truncating or blank-padding
    /// every row (including the stashed alt screen) to match and clamping
    /// the cursor into the new bounds. Existing per-row content is kept; no
    /// reflow of wrapped lines is performed. A no-op when `width` is
    /// unchanged or zero.
    pub fn resize(&mut self, width: u16) {
        if width == 0 || width == self.width {
            return;
        }
        self.width = width;
        let cols = width as usize;
        for row in &mut self.cells {
            row.resize(cols, StyledCell::default());
        }
        if let Some(saved) = self.saved_screen.as_mut() {
            for row in &mut saved.cells {
                row.resize(cols, StyledCell::default());
            }
            saved.cursor_col = saved.cursor_col.min(cols);
        }
        self.cursor_col = self.cursor_col.min(cols);
        if let Some((_, col)) = self.saved_cursor.as_mut() {
            *col = (*col).min(cols);
        }
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
        for row in self.cells.range(row_start..row_end) {
            let slice_end = col_end.min(row.len());
            let raw: String = if col_start >= slice_end {
                String::new()
            } else {
                row[col_start..slice_end].iter().map(|c| c.ch).collect()
            };
            lines.push(raw.trim_end().to_string());
        }
        lines.join("\n")
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        for &byte in bytes {
            parser.advance(self, byte);
        }
        self.parser = parser;
    }

    /// Grow `cells` so `row` is addressable, then enforce the scrollback
    /// cap. Only ever called with `cursor_row`, so eviction subtracts the
    /// evicted count from `cursor_row` to keep it on the same row.
    fn ensure_row(&mut self, row: usize) {
        while self.cells.len() <= row {
            self.cells
                .push_back(vec![StyledCell::default(); self.width as usize]);
        }
        let evicted = self.cells.len().saturating_sub(self.scrollback_limit);
        if evicted > 0 {
            self.cells.drain(0..evicted);
            self.cursor_row = self.cursor_row.saturating_sub(evicted);
        }
    }

    /// Advance the cursor one row. Inside a scroll region whose bottom
    /// margin the cursor sits on, scroll the region's rows up by one
    /// instead of growing the buffer.
    fn line_feed(&mut self) {
        if let Some((top, bottom)) = self.scroll_region {
            if self.cursor_row >= bottom {
                self.scroll_region_up(top, bottom);
                return;
            }
        }
        self.cursor_row += 1;
        self.ensure_row(self.cursor_row);
    }

    /// Drop the top margin row and insert a blank at the bottom margin,
    /// scrolling `top..=bottom` up by one and leaving rows outside the
    /// region untouched.
    fn scroll_region_up(&mut self, top: usize, bottom: usize) {
        if top >= self.cells.len() || top > bottom {
            return;
        }
        self.cells.remove(top);
        let insert_at = bottom.min(self.cells.len());
        self.cells
            .insert(insert_at, vec![StyledCell::default(); self.width as usize]);
    }

    fn put_char(&mut self, ch: char) {
        let w = self.width as usize;
        self.ensure_row(self.cursor_row);
        if self.cursor_col < w {
            self.cells[self.cursor_row][self.cursor_col] = StyledCell {
                ch,
                fg: self.pen_fg,
                bg: self.pen_bg,
                modifiers: self.pen_modifiers,
            };
            self.cursor_col += 1;
        }
    }

    fn reset_pen(&mut self) {
        self.pen_fg = None;
        self.pen_bg = None;
        self.pen_modifiers = TermModifier::empty();
    }
}

impl vte::Perform for VtermGrid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.cursor_col = 0;
                self.line_feed();
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
        let code = params.first().copied().unwrap_or_default();

        // OSC 7 -- the program's working directory as a file URI:
        // ESC ] 7 ; file://<host>/<path> ST. Overwrite the tracked cwd.
        if code == b"7" {
            if let Some(path) = params.get(1).copied().and_then(parse_osc7_cwd) {
                self.cwd = Some(path);
            }
            return;
        }

        // OSC 133 -- shell-integration command marks: ESC ] 133 ; C ST
        // (command output start) and ESC ] 133 ; D [ ; <exit> ] ST
        // (command done). PS0/PROMPT_COMMAND in the spawned shell emit
        // these, so command boundaries need no echoed sentinel line.
        if code == b"133" {
            match params.get(1).copied() {
                Some(b"C") => self.command_marks.push(CommandMark::Start),
                Some(b"D") => {
                    let exit = params
                        .get(2)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse().ok());
                    self.command_marks.push(CommandMark::Done { exit });
                },
                _ => {},
            }
            return;
        }

        // OSC 52 -- "set clipboard". Format: ESC ] 52 ; <Pc> ; <Pd> ST,
        // where <Pc> is the selection ("c" / "p" / "s" / mixed / empty)
        // and <Pd> is base64-encoded text. We honour writes targeted at
        // the system clipboard ("c", empty, or mixed including "c") and
        // ignore primary-only ("p") since the editor has no separate
        // primary-selection plumbing yet.
        if params.len() < 3 || code != b"52" {
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

        if intermediates == [b'?'] && (action == 'h' || action == 'l') {
            self.set_private_modes(&params_vec, action == 'h');
            return;
        }

        if intermediates == [b' '] && action == 'q' {
            self.cursor_shape = match first_param(&params_vec, 0) {
                3 | 4 => CursorShape::Underline,
                5 | 6 => CursorShape::Bar,
                _ => CursorShape::Block,
            };
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
                        1 => self.pen_modifiers |= TermModifier::BOLD,
                        2 => self.pen_modifiers |= TermModifier::DIM,
                        3 => self.pen_modifiers |= TermModifier::ITALIC,
                        4 => self.pen_modifiers |= TermModifier::UNDERLINED,
                        7 => self.pen_modifiers |= TermModifier::REVERSED,
                        9 => self.pen_modifiers |= TermModifier::CROSSED_OUT,
                        22 => {
                            self.pen_modifiers -= TermModifier::BOLD;
                            self.pen_modifiers -= TermModifier::DIM;
                        },
                        23 => self.pen_modifiers -= TermModifier::ITALIC,
                        24 => self.pen_modifiers -= TermModifier::UNDERLINED,
                        27 => self.pen_modifiers -= TermModifier::REVERSED,
                        29 => self.pen_modifiers -= TermModifier::CROSSED_OUT,
                        30 => self.pen_fg = Some(TermColor::Black),
                        31 => self.pen_fg = Some(TermColor::Red),
                        32 => self.pen_fg = Some(TermColor::Green),
                        33 => self.pen_fg = Some(TermColor::Yellow),
                        34 => self.pen_fg = Some(TermColor::Blue),
                        35 => self.pen_fg = Some(TermColor::Magenta),
                        36 => self.pen_fg = Some(TermColor::Cyan),
                        37 => self.pen_fg = Some(TermColor::White),
                        38 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_fg = Some(TermColor::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        38 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_fg = Some(TermColor::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        39 => self.pen_fg = None,
                        40 => self.pen_bg = Some(TermColor::Black),
                        41 => self.pen_bg = Some(TermColor::Red),
                        42 => self.pen_bg = Some(TermColor::Green),
                        43 => self.pen_bg = Some(TermColor::Yellow),
                        44 => self.pen_bg = Some(TermColor::Blue),
                        45 => self.pen_bg = Some(TermColor::Magenta),
                        46 => self.pen_bg = Some(TermColor::Cyan),
                        47 => self.pen_bg = Some(TermColor::White),
                        48 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_bg = Some(TermColor::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        48 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_bg = Some(TermColor::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        49 => self.pen_bg = None,
                        90 => self.pen_fg = Some(TermColor::DarkGray),
                        91 => self.pen_fg = Some(TermColor::LightRed),
                        92 => self.pen_fg = Some(TermColor::LightGreen),
                        93 => self.pen_fg = Some(TermColor::LightYellow),
                        94 => self.pen_fg = Some(TermColor::LightBlue),
                        95 => self.pen_fg = Some(TermColor::LightMagenta),
                        96 => self.pen_fg = Some(TermColor::LightCyan),
                        97 => self.pen_fg = Some(TermColor::White),
                        100 => self.pen_bg = Some(TermColor::DarkGray),
                        101 => self.pen_bg = Some(TermColor::LightRed),
                        102 => self.pen_bg = Some(TermColor::LightGreen),
                        103 => self.pen_bg = Some(TermColor::LightYellow),
                        104 => self.pen_bg = Some(TermColor::LightBlue),
                        105 => self.pen_bg = Some(TermColor::LightMagenta),
                        106 => self.pen_bg = Some(TermColor::LightCyan),
                        107 => self.pen_bg = Some(TermColor::White),
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
            'H' | 'f' => {
                let row = first_param(&params_vec, 1) as usize;
                let col = param_at(&params_vec, 1, 1) as usize;
                self.cursor_row = row - 1;
                self.cursor_col = (col - 1).min(self.width as usize - 1);
                self.ensure_row(self.cursor_row);
            },
            'r' => {
                if params_vec.len() >= 2 {
                    let top = first_param(&params_vec, 1) as usize;
                    let bottom = param_at(&params_vec, 1, 1) as usize;
                    self.scroll_region = (top < bottom).then(|| (top - 1, bottom - 1));
                } else {
                    self.scroll_region = None;
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
            },
            's' => {
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
            },
            'u' => {
                if let Some((row, col)) = self.saved_cursor {
                    self.cursor_row = row;
                    self.cursor_col = col;
                    self.ensure_row(self.cursor_row);
                }
            },
            'K' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                let row = &mut self.cells[self.cursor_row];
                match mode {
                    0 => {
                        for cell in row.iter_mut().take(w).skip(self.cursor_col) {
                            *cell = StyledCell::default();
                        }
                    },
                    1 => {
                        for cell in row.iter_mut().take(self.cursor_col.min(w - 1) + 1) {
                            *cell = StyledCell::default();
                        }
                    },
                    2 => {
                        for cell in row.iter_mut() {
                            *cell = StyledCell::default();
                        }
                    },
                    _ => {},
                }
            },
            'J' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                match mode {
                    0 => {
                        for col in self.cursor_col..w {
                            self.cells[self.cursor_row][col] = StyledCell::default();
                        }
                        for row in (self.cursor_row + 1)..self.cells.len() {
                            for cell in &mut self.cells[row] {
                                *cell = StyledCell::default();
                            }
                        }
                    },
                    1 => {
                        for row in 0..self.cursor_row {
                            for cell in &mut self.cells[row] {
                                *cell = StyledCell::default();
                            }
                        }
                        for col in 0..=self.cursor_col.min(w - 1) {
                            self.cells[self.cursor_row][col] = StyledCell::default();
                        }
                    },
                    2 => {
                        for row in &mut self.cells {
                            for cell in row.iter_mut() {
                                *cell = StyledCell::default();
                            }
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

/// Extract the path from an OSC 7 `file://<host>/<path>` URI, percent-decoded.
/// `None` for a non-`file` scheme or non-UTF-8 input.
fn parse_osc7_cwd(uri: &[u8]) -> Option<String> {
    let uri = std::str::from_utf8(uri).ok()?;
    let after_scheme = uri.strip_prefix("file://")?;
    let path_start = after_scheme.find('/')?;
    Some(percent_decode(&after_scheme[path_start..]))
}

/// Decode `%XX` escapes in `s`, passing other bytes through.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&hi), Some(&lo)) => match (hex_val(hi), hex_val(lo)) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                },
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                },
            },
            _ => {
                out.push(bytes[i]);
                i += 1;
            },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Inclusive single-row selection spanning the byte range `[start, end)`
/// of a row's text, with byte offsets mapped to cell columns.
fn selection_for(text: &str, start: usize, end: usize, row: u16) -> GridSelection {
    let anchor_col = text[..start].chars().count() as u16;
    let head_col = (text[..end].chars().count().max(1) - 1) as u16;
    GridSelection {
        anchor: (anchor_col, row),
        head: (head_col, row),
    }
}

fn first_param(params: &[u16], default: u16) -> u16 {
    param_at(params, 0, default)
}

/// The `idx`th CSI parameter, falling back to `default` when absent or
/// zero (a zero parameter means "use the default" per the spec).
fn param_at(params: &[u16], idx: usize, default: u16) -> u16 {
    params
        .get(idx)
        .copied()
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

/// Encode a mouse event. SGR (`?1006`) carries the button and 0-based
/// position as decimals with an `M`/`m` press/release suffix; X10 packs
/// them into single bytes offset by 32 and is limited to positions under
/// 223, with button 3 standing in for any release.
fn encode_mouse_report(
    sgr: bool,
    button: u8,
    mods: u8,
    col: u16,
    row: u16,
    pressed: bool,
) -> Option<Vec<u8>> {
    let cb = button + mods;
    if sgr {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, suffix).into_bytes());
    }
    if col >= 223 || row >= 223 {
        return None;
    }
    let cb = if pressed { cb } else { 3 + mods };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + cb,
        32 + 1 + col as u8,
        32 + 1 + row as u8,
    ])
}

/// A two-anchor selection over a [`VtermGrid`]. Coordinates are
/// `(col, row)` cell positions in the grid's coordinate space, not
/// terminal-relative -- consumers translate from screen coords to grid
/// coords before constructing the selection. `anchor` is the click
/// position; `head` follows the drag. [`Self::bounds`] normalizes the
/// pair for row-major iteration regardless of drag direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSelection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

/// A clickable target detected in the grid by [`VtermGrid::links`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// An `http`/`https` URL.
    Url(String),
    /// A file path with an optional `line` and `column`.
    Path {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

/// A detected link: the cells it spans and where it points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLink {
    pub selection: GridSelection,
    pub target: LinkTarget,
}

impl GridSelection {
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
}

pub struct OutputBlock {
    pub command: String,
    pub grid: VtermGrid,
    pub finished: bool,
    pub exit_status: Option<i32>,
    pub error: Option<String>,
    /// Active selection over [`Self::grid`]. `None` means no selection;
    /// populated by mouse-drag handlers and consumed by the run-pane
    /// renderer to paint reverse-video over the covered cells.
    pub selection: Option<GridSelection>,
}

impl OutputBlock {
    pub fn new(command: String, width: u16) -> Self {
        Self {
            command,
            grid: VtermGrid::new(width),
            finished: false,
            exit_status: None,
            error: None,
            selection: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
    }

    pub fn status(&self) -> BlockStatus {
        if !self.finished {
            BlockStatus::Running
        } else if self.exit_status == Some(0) {
            BlockStatus::Succeeded
        } else {
            BlockStatus::Failed(self.exit_status)
        }
    }
}

/// Lifecycle state of an [`OutputBlock`], derived from its
/// `finished`/`exit_status`. Drives the run-pane status affordance in
/// both the TUI and GUI renderers, which map each variant to a theme
/// color; [`BlockStatus::label`] supplies the shared marker text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
    Running,
    Succeeded,
    Failed(Option<i32>),
}

impl BlockStatus {
    /// Marker shown beneath the block. Finished commands render their
    /// exit code; a command the shell ended without reporting a code
    /// renders as `[exit ?]`.
    pub fn label(self) -> String {
        match self {
            BlockStatus::Running => "[running]".to_string(),
            BlockStatus::Succeeded => "[exit 0]".to_string(),
            BlockStatus::Failed(Some(code)) => format!("[exit {code}]"),
            BlockStatus::Failed(None) => "[exit ?]".to_string(),
        }
    }
}
