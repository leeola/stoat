//! An interactive multi-pool smooth-scroll stoatty demo: two side-by-side panes
//! and a togglable overlay, each a separate `Gstoatty` pool that scrolls
//! independently, so several smooth scrolls run and composite at once.
//!
//! The viewport is framed with static VT chrome -- a title bar, a footer, and a
//! vertical divider between the panes -- written once to the live grid. Each pane
//! is its own pool (`Gstoatty;pool_region` with a distinct id) over a numbered
//! document streamed into that pool's recycled page slots. The mouse wheel
//! scrolls whichever pool the pointer is over, so the two panes glide at the same
//! time and independently; `o` toggles an overlay pool that composites on top of
//! both (a higher id is a higher z-order) and is retired with `Gstoatty;pool_drop`
//! when hidden.
//!
//! Each pool also paints its visible rows into the live grid as the resting "live
//! screen" the renderer hands back to once a glide settles, so the demo is a
//! plainly scrolling split view in any terminal that ignores the pool frames.
//!
//! Runs in raw mode with mouse reporting on. Ctrl-F / Ctrl-B page the active pool
//! a whole region at a time; `o` toggles the overlay; `q` or Ctrl-C quits. Run as
//! the PTY shell by the `smooth_scroll_pages` example.

use ratatui::crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io::{self, Write};
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_pool_drop_into, encode_pool_region_into,
    encode_scroll_into, PoolRegionCommand, ScrollCommand,
};

/// Viewport size in cells, matching the window the `smooth_scroll_pages` example
/// opens.
const COLS: usize = 80;
const VIEWPORT_H: u16 = 24;

/// The two panes share a top row (below the title) and height (above the footer);
/// a divider column splits them.
const PANE_TOP: u16 = 1;
const PANE_HEIGHT: u16 = 22;
const PANE_WIDTH: u16 = 38;
const LEFT_LEFT: u16 = 1;
const DIVIDER_COL: u16 = LEFT_LEFT + PANE_WIDTH;
const RIGHT_LEFT: u16 = DIVIDER_COL + 1;

/// The overlay pool: a smaller rectangle floating over the panes, toggled with
/// `o`. Its higher id puts it above the panes in the renderer's z-order.
const OVERLAY_TOP: u16 = 6;
const OVERLAY_LEFT: u16 = 20;
const OVERLAY_WIDTH: u16 = 40;
const OVERLAY_HEIGHT: u16 = 12;

/// Pool ids. Ascending id is ascending z-order, so the overlay composites last.
const LEFT_POOL: u32 = 1;
const RIGHT_POOL: u32 = 2;
const OVERLAY_POOL: u32 = 3;

/// Pages kept buffered around each pool's cursor, the pool's capacity.
const WINDOW_PAGES: u64 = 5;

/// Rows a single wheel notch scrolls, a sub-page step stoatty eases across.
const STEP_ROWS: f32 = 3.0;

/// Pages a single Ctrl-F / Ctrl-B press skips, a full region like a pager's page
/// key.
const PAGE_STEP: f32 = 1.0;

/// Per-pool document background, distinct so the two panes and the overlay read
/// apart while scrolling.
const LEFT_BG: [u8; 3] = [40, 44, 52];
const RIGHT_BG: [u8; 3] = [33, 37, 45];
const OVERLAY_BG: [u8; 3] = [58, 48, 38];

/// Body text (`#abb2bf`, One Dark foreground) and section-header (`#61afef`, One
/// Dark blue) colors, shared by every pool's document.
const BODY_FG: [u8; 3] = [171, 178, 191];
const HEADER_FG: [u8; 3] = [97, 175, 239];

/// Chrome foreground (`#e5c07b`, One Dark yellow) for the title, footer, and
/// divider.
const CHROME_FG: [u8; 3] = [229, 192, 123];

/// One scrollable pool: a declared region over a numbered document, with the
/// fractional scroll position and the buffered page window for that pool.
struct Pool {
    id: u32,
    region: PoolRegionCommand,
    bg: [u8; 3],
    label: &'static str,
    /// Scroll position in document pages; a page is `region.height` rows.
    position: f32,
    /// Half-open page range filled into the pool, refilled only when the integer
    /// page changes so a sub-page move reuses the buffered pages.
    window_start: Option<u64>,
}

impl Pool {
    fn new(
        id: u32,
        top: u16,
        left: u16,
        width: u16,
        height: u16,
        bg: [u8; 3],
        label: &'static str,
    ) -> Pool {
        Pool {
            id,
            region: PoolRegionCommand {
                pool: id,
                top,
                left,
                width,
                height,
            },
            bg,
            label,
            position: 0.0,
            window_start: None,
        }
    }

    fn rows(&self) -> usize {
        self.region.height as usize
    }

    fn step(&self) -> f32 {
        STEP_ROWS / self.rows() as f32
    }

    fn scroll_by(&mut self, pages: f32) {
        self.position = (self.position + pages).max(0.0);
    }

    /// Declare the pool's region, so its later fills and scrolls have a pool to
    /// land in. Sent once when the pool first appears.
    fn declare(&self, out: &mut Vec<u8>) {
        encode_pool_region_into(out, &self.region);
    }

    /// Paint the visible rows into the live grid, refill the buffered window, and
    /// report the scroll target: the per-frame work for one pool.
    fn emit(&mut self, out: &mut Vec<u8>) {
        self.paint_live(out);
        self.refill(out);
        self.emit_scroll(out);
    }

    /// Paint the document rows currently under `position` into the live grid's
    /// region, the "live screen" the renderer shows whenever the glide rests and
    /// the degradation any non-stoatty terminal renders.
    fn paint_live(&self, out: &mut Vec<u8>) {
        let start = (self.position * self.rows() as f32).floor() as usize;
        for r in 0..self.rows() {
            let (fg, text) = document_line(self.label, start + r);
            let row = self.region.top + 1 + r as u16;
            let col = self.region.left + 1;
            let _ = write!(out, "\x1b[{row};{col}H");
            write_line(out, fg, self.bg, self.region.width as usize, &text);
        }
    }

    fn refill(&mut self, out: &mut Vec<u8>) {
        let start = (self.position as u64).saturating_sub(WINDOW_PAGES / 2);
        if self.window_start == Some(start) {
            return;
        }
        self.window_start = Some(start);

        for page in start..start + WINDOW_PAGES {
            encode_fill_into(out, self.id, page);
            self.write_page(out, page);
            encode_fill_end_into(out);
        }
    }

    /// Stream one region of the document into the pool slot for `page`, homing the
    /// cursor first so the bytes paint a fresh slot sized to the region.
    fn write_page(&self, out: &mut Vec<u8>, page: u64) {
        out.extend_from_slice(b"\x1b[H");
        for row in 0..self.rows() {
            let (fg, text) = document_line(self.label, page as usize * self.rows() + row);
            write_line(out, fg, self.bg, self.region.width as usize, &text);
            if row + 1 < self.rows() {
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    fn emit_scroll(&self, out: &mut Vec<u8>) {
        let page = self.position.floor();
        let fraction = ((self.position - page) * 65536.0) as u16;
        encode_scroll_into(
            out,
            &ScrollCommand {
                pool: self.id,
                page: page as u64,
                fraction,
            },
        );
    }
}

fn main() {
    enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnableMouseCapture).expect("enable mouse capture");
    let _ = stdout.write_all(b"\x1b[?25l");
    let _ = stdout.flush();

    run();

    let _ = execute!(stdout, DisableMouseCapture);
    let _ = stdout.write_all(b"\x1b[?25h");
    let _ = stdout.flush();
    disable_raw_mode().ok();
}

/// Scroll the panes and overlay under mouse and key control until the user quits,
/// returning so [`main`] can restore the terminal.
fn run() {
    let mut left = Pool::new(
        LEFT_POOL,
        PANE_TOP,
        LEFT_LEFT,
        PANE_WIDTH,
        PANE_HEIGHT,
        LEFT_BG,
        "L",
    );
    let mut right = Pool::new(
        RIGHT_POOL,
        PANE_TOP,
        RIGHT_LEFT,
        PANE_WIDTH,
        PANE_HEIGHT,
        RIGHT_BG,
        "R",
    );
    let mut overlay: Option<Pool> = None;
    let mut active = LEFT_POOL;

    let mut out = Vec::new();
    write_chrome(&mut out);
    left.declare(&mut out);
    right.declare(&mut out);
    left.emit(&mut out);
    right.emit(&mut out);
    flush(&mut out);

    loop {
        match event::read().expect("read a terminal event") {
            Event::Mouse(mouse) => {
                active = pool_at(mouse.column, mouse.row, overlay.is_some());
                let dir = match mouse.kind {
                    MouseEventKind::ScrollDown => 1.0,
                    MouseEventKind::ScrollUp => -1.0,
                    _ => continue,
                };
                with_active(active, &mut left, &mut right, &mut overlay, |pool| {
                    pool.scroll_by(dir * pool.step())
                });
            },
            Event::Key(key) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if ctrl => break,
                    KeyCode::Char('o') => toggle_overlay(&mut overlay, &mut active, &mut out),
                    KeyCode::Char('f') if ctrl => {
                        with_active(active, &mut left, &mut right, &mut overlay, |pool| {
                            pool.scroll_by(PAGE_STEP)
                        });
                    },
                    KeyCode::Char('b') if ctrl => {
                        with_active(active, &mut left, &mut right, &mut overlay, |pool| {
                            pool.scroll_by(-PAGE_STEP)
                        });
                    },
                    _ => continue,
                }
            },
            _ => continue,
        }

        left.emit(&mut out);
        right.emit(&mut out);
        if let Some(overlay) = overlay.as_mut() {
            overlay.emit(&mut out);
        }
        flush(&mut out);
    }
}

/// Run `f` on the pool `active` names, ignoring it when the overlay is the active
/// pool but currently hidden.
fn with_active(
    active: u32,
    left: &mut Pool,
    right: &mut Pool,
    overlay: &mut Option<Pool>,
    f: impl FnOnce(&mut Pool),
) {
    match active {
        RIGHT_POOL => f(right),
        OVERLAY_POOL => {
            if let Some(overlay) = overlay.as_mut() {
                f(overlay);
            }
        },
        _ => f(left),
    }
}

/// Show or hide the overlay pool. Showing declares its region and makes it
/// active; hiding retires it with `pool_drop` and repaints the chrome, so the
/// divider cells it covered are restored while the panes' own repaint covers the
/// rest.
fn toggle_overlay(overlay: &mut Option<Pool>, active: &mut u32, out: &mut Vec<u8>) {
    match overlay.take() {
        Some(_) => {
            encode_pool_drop_into(out, OVERLAY_POOL);
            write_chrome(out);
            *active = LEFT_POOL;
        },
        None => {
            let pool = Pool::new(
                OVERLAY_POOL,
                OVERLAY_TOP,
                OVERLAY_LEFT,
                OVERLAY_WIDTH,
                OVERLAY_HEIGHT,
                OVERLAY_BG,
                "OVL",
            );
            pool.declare(out);
            *overlay = Some(pool);
            *active = OVERLAY_POOL;
        },
    }
}

/// The pool the pointer at (`col`, `row`) sits over: the overlay when shown and
/// hit, otherwise the pane on that side of the divider.
fn pool_at(col: u16, row: u16, overlay_shown: bool) -> u32 {
    if overlay_shown
        && (OVERLAY_LEFT..OVERLAY_LEFT + OVERLAY_WIDTH).contains(&col)
        && (OVERLAY_TOP..OVERLAY_TOP + OVERLAY_HEIGHT).contains(&row)
    {
        OVERLAY_POOL
    } else if col < DIVIDER_COL {
        LEFT_POOL
    } else {
        RIGHT_POOL
    }
}

/// Paint the static frame onto the live grid: a reversed title bar on the top
/// row, a reversed footer on the bottom row, and a vertical divider between the
/// panes. Ordinary VT, so it stays fixed while the pooled rows scroll and renders
/// as a plain framed split in any other terminal.
fn write_chrome(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[H");
    let _ = write!(
        out,
        "\x1b[7;38;2;{};{};{}m",
        CHROME_FG[0], CHROME_FG[1], CHROME_FG[2],
    );
    let title = " stoatty multi-pool smooth scroll  (wheel a pane, o = overlay, q quits) ";
    let _ = write!(out, "{title:<COLS$}");
    out.extend_from_slice(b"\x1b[0m");

    for r in 0..PANE_HEIGHT {
        let row = PANE_TOP + 1 + r;
        let _ = write!(
            out,
            "\x1b[{};{}H\x1b[38;2;{};{};{}m\u{2502}\x1b[0m",
            row,
            DIVIDER_COL + 1,
            CHROME_FG[0],
            CHROME_FG[1],
            CHROME_FG[2],
        );
    }

    let _ = write!(
        out,
        "\x1b[{};1H\x1b[7;38;2;{};{};{}m",
        VIEWPORT_H, CHROME_FG[0], CHROME_FG[1], CHROME_FG[2],
    );
    let footer = " left + right panes scroll independently; the overlay floats on top ";
    let _ = write!(out, "{footer:<COLS$}");
    out.extend_from_slice(b"\x1b[0m");
}

/// The foreground color and text for document row `d` (zero-based) of the pool
/// labelled `label`: a section header every tenth row, a numbered body line
/// otherwise.
fn document_line(label: &str, d: usize) -> ([u8; 3], String) {
    let line = d + 1;
    if d.is_multiple_of(10) {
        (HEADER_FG, format!("== {label} section {} ==", d / 10))
    } else {
        (BODY_FG, format!("{label} line {line}"))
    }
}

/// Append one row of `text` in `fg` over `bg`, padded to `width` so it overwrites
/// whatever the row held before.
fn write_line(out: &mut Vec<u8>, fg: [u8; 3], bg: [u8; 3], width: usize, text: &str) {
    let _ = write!(
        out,
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        fg[0], fg[1], fg[2], bg[0], bg[1], bg[2],
    );

    let mut text = text.to_string();
    text.truncate(width);
    let _ = write!(out, "{text:<width$}");

    out.extend_from_slice(b"\x1b[0m");
}

/// Write the accumulated bytes to stdout and clear the buffer for the next batch.
fn flush(out: &mut Vec<u8>) {
    let mut stdout = io::stdout();
    stdout.write_all(out).expect("write to stdout");
    stdout.flush().expect("flush stdout");
    out.clear();
}
