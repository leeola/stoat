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
//! One [`SmoothScrollState`] tracks all three pools, and every pool emits
//! through [`pool::emit_into`], which owns the region declaration, the buffered
//! page window, and the scroll target. Each pool contributes only its own
//! document bytes and where it is scrolled to.
//!
//! Each pool also paints its visible rows into the live grid as the resting "live
//! screen" the renderer hands back to once a glide settles. That paint is plain
//! VT, so the demo is a plainly scrolling split view anywhere else. Off a
//! stoatty the pooled lane is withheld entirely rather than ignored, because a
//! page's cells stream outside the APC wrapper and print over the screen.
//!
//! The cursor rides the active pool. `Gstoatty;pool_cursor` names the pool and
//! the document row it sits on, and the terminal draws it from that frame while
//! the pool eases, so the cursor glides with the text rather than holding its
//! screen position. `x` withholds the frame, which leaves the plain VT cursor
//! resting where the last paint put it.
//!
//! The overlay rides the left pane. `Gstoatty;pool_anchor` names the host and
//! the document row the layout assumed, and the panel and title run carry the
//! same anchor, so the whole note travels with the pane's eased offset rather
//! than holding still over moving text. `a` drops the anchor, which leaves the
//! note screen-fixed for comparison.
//!
//! Runs in raw mode with mouse reporting on. Ctrl-F / Ctrl-B page the active
//! pool a whole region at a time; `j` and `k` move its cursor; `o` toggles the
//! overlay; `a` toggles the overlay's anchor; `x` toggles the cursor anchor;
//! `q` or Ctrl-C quits. Run as the PTY shell by the `smooth_scroll_pages`
//! example.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind};
use std::io::{self, Write};
use stoat_widgets::{
    pool::{self, SmoothScrollState},
    ApcSession, SessionOptions,
};
use stoatty_protocol::command::{
    encode_panel_into, encode_pool_anchor_into, encode_pool_cursor_into, encode_reset_into,
    encode_text_run_into, BorderStyle, PanelCommand, PanelShadow, PoolAnchorCommand,
    PoolCursorCommand, PoolRegionCommand, TextRunCommand,
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

/// One scrollable pool, a declared region over a numbered document.
///
/// The pool holds only what the document looks like and where it is scrolled to.
/// What has been declared to the terminal for it -- the region, the buffered
/// pages, the last scroll target -- lives in the shared [`SmoothScrollState`]
/// every pool emits through, keyed by [`Self::id`].
struct Pool {
    id: u32,
    region: PoolRegionCommand,
    bg: [u8; 3],
    label: &'static str,
    /// Scroll position in document pages; a page is `region.height` rows.
    position: f32,
    /// Document row the cursor sits on, which the anchor frame names and the
    /// live paint rests the VT cursor at.
    cursor_line: usize,
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
                window: 0,
            },
            bg,
            label,
            position: 0.0,
            cursor_line: 0,
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

    /// Move the cursor by `lines`, bringing the viewport with it when it leaves
    /// the visible rows.
    ///
    /// Scrolling by one step rather than jumping the page is what puts the
    /// glide and the cursor in motion together, which is the thing the anchor
    /// frame exists to keep aligned.
    fn move_cursor(&mut self, lines: isize) {
        self.cursor_line = self.cursor_line.saturating_add_signed(lines);

        let top = self.top_line();
        if self.cursor_line < top {
            self.scroll_by(-self.step());
        } else if self.cursor_line >= top + self.rows() {
            self.scroll_by(self.step());
        }
    }

    /// The first document row the viewport shows.
    fn top_line(&self) -> usize {
        (self.position * self.rows() as f32).floor() as usize
    }

    /// Paint the visible rows into the live grid, and on a stoatty declare the
    /// region, refill the buffered window, and report the scroll target.
    ///
    /// The live paint is plain VT and goes out to any host. The pooled work is
    /// withheld from the rest, because a page's cells stream outside the APC
    /// wrapper and a terminal that never opened the fill prints them over the
    /// screen.
    ///
    /// The document never changes, so the content version is a constant, and the
    /// demo emits only on an event rather than every frame, so it does not hold
    /// while idle.
    fn emit(&self, out: &mut Vec<u8>, state: &mut SmoothScrollState, live: bool, anchor: bool) {
        self.paint_live(out);
        if !live {
            return;
        }

        pool::emit_into(
            out,
            state,
            self.region,
            self.position * self.rows() as f32,
            0,
            false,
            |page| self.page_bytes(page),
        );

        // The frame names a document row, so the terminal keeps the cursor on
        // its line through the ease rather than at the screen position the last
        // paint left it at. Withheld under `x`, which is what makes the two
        // behaviors comparable.
        if anchor {
            encode_pool_cursor_into(
                out,
                &PoolCursorCommand {
                    pool: self.id,
                    row: self.cursor_line as u64,
                    col: self.region.left + 1,
                },
            );
        }
    }

    /// Paint the document rows currently under `position` into the live grid's
    /// region, the "live screen" the renderer shows whenever the glide rests and
    /// the degradation any non-stoatty terminal renders.
    ///
    /// The cursor rests on its own line when that line is on the page, so a host
    /// that draws no anchor frame still shows it somewhere sensible.
    fn paint_live(&self, out: &mut Vec<u8>) {
        let start = self.top_line();
        for r in 0..self.rows() {
            let (fg, text) = document_line(self.label, start + r);
            let row = self.region.top + 1 + r as u16;
            let col = self.region.left + 1;
            let _ = write!(out, "\x1b[{row};{col}H");
            write_line(out, fg, self.bg, self.region.width as usize, &text);
        }

        if let Some(offset) = self
            .cursor_line
            .checked_sub(start)
            .filter(|r| *r < self.rows())
        {
            let row = self.region.top + 1 + offset as u16;
            let col = self.region.left + 1;
            let _ = write!(out, "\x1b[{row};{col}H");
        }
    }

    /// One region of the document as the self-contained VT bytes painting the
    /// pool slot for `page`, homing the cursor first so they paint a fresh slot
    /// sized to the region.
    fn page_bytes(&self, page: u64) -> Vec<u8> {
        let mut out = Vec::from(b"\x1b[H".as_slice());
        for row in 0..self.rows() {
            let (fg, text) = document_line(self.label, page as usize * self.rows() + row);
            write_line(&mut out, fg, self.bg, self.region.width as usize, &text);
            if row + 1 < self.rows() {
                out.extend_from_slice(b"\r\n");
            }
        }
        out
    }
}

fn main() {
    // The session holds raw mode and mouse reporting for as long as it lives,
    // and gives both back on the way out of main -- including the way out an
    // `expect` below takes, which its panic hook covers.
    let session = ApcSession::new(SessionOptions {
        raw_mode: true,
        mouse_capture: true,
        // Shown, because the cursor is the thing the anchor frame moves.
        hide_cursor: false,
        ..SessionOptions::default()
    });

    run(session.live());
}

/// Scroll the panes and overlay under mouse and key control until the user quits,
/// returning so [`main`]'s session restores the terminal.
///
/// `live` is whether the host renders pools. Without it the panes still scroll,
/// but only through the plain rows each pool paints into the live grid.
fn run(live: bool) {
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
    // Withheld under `x`, so the plain VT cursor holding its screen position
    // through a glide reads against the anchored one.
    let mut anchor = true;
    // Withheld under `a`, so a note fixed to the screen while the pane eases
    // reads against one that travels with it.
    let mut ridden = true;
    let mut state = SmoothScrollState::default();

    let mut out = Vec::new();
    write_chrome(&mut out);
    left.emit(&mut out, &mut state, live, anchor && active == LEFT_POOL);
    right.emit(&mut out, &mut state, live, anchor && active == RIGHT_POOL);
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
                    KeyCode::Char('o') => {
                        let host_top = left.position * left.rows() as f32;
                        toggle_overlay(&mut overlay, &mut active, &mut out, live, ridden, host_top);
                    },
                    KeyCode::Char('a') => {
                        // An anchor clears only with its pool, so the overlay
                        // is dropped and re-declared to change form.
                        ridden = !ridden;
                        let host_top = left.position * left.rows() as f32;
                        if overlay.is_some() {
                            toggle_overlay(
                                &mut overlay,
                                &mut active,
                                &mut out,
                                live,
                                ridden,
                                host_top,
                            );
                            toggle_overlay(
                                &mut overlay,
                                &mut active,
                                &mut out,
                                live,
                                ridden,
                                host_top,
                            );
                        }
                    },
                    KeyCode::Char('x') => anchor = !anchor,
                    KeyCode::Char('j') => {
                        with_active(active, &mut left, &mut right, &mut overlay, |pool| {
                            pool.move_cursor(1)
                        });
                    },
                    KeyCode::Char('k') => {
                        with_active(active, &mut left, &mut right, &mut overlay, |pool| {
                            pool.move_cursor(-1)
                        });
                    },
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

        left.emit(&mut out, &mut state, live, anchor && active == LEFT_POOL);
        right.emit(&mut out, &mut state, live, anchor && active == RIGHT_POOL);
        if let Some(overlay) = overlay.as_ref() {
            overlay.emit(&mut out, &mut state, live, anchor && active == OVERLAY_POOL);

            // The anchor ships every tick, so the note stays frame-locked to
            // the pane it rides while that pane eases.
            if live && ridden {
                encode_pool_anchor_into(
                    &mut out,
                    &PoolAnchorCommand {
                        pool: OVERLAY_POOL,
                        host: LEFT_POOL,
                        top_rows: left.position * left.rows() as f32,
                    },
                );
            }
        }

        // A surface is retired by leaving its id out of the declared set, so a
        // hidden overlay drops here rather than at the keypress that hid it.
        let mut declared = vec![LEFT_POOL, RIGHT_POOL];
        declared.extend(overlay.as_ref().map(|overlay| overlay.id));
        state.drop_absent(&mut out, &declared);

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

/// Show or hide the overlay, writing the chrome that frames it.
///
/// `ridden` puts the overlay's frame and title on the left pane's anchor, so
/// they travel with the pane's eased offset. `host_top` is the document row the
/// layout assumed, which the terminal measures that offset against.
///
/// A reset clears decorations and leaves the pools standing, so the chrome is
/// re-sent whole on each toggle rather than tracked frame by frame. It is
/// withheld off a stoatty, like every other frame this demo sends.
fn toggle_overlay(
    overlay: &mut Option<Pool>,
    active: &mut u32,
    out: &mut Vec<u8>,
    live: bool,
    ridden: bool,
    host_top: f32,
) {
    match overlay.take() {
        Some(_) => {
            if live {
                encode_reset_into(out);
            }
            write_chrome(out);
            *active = LEFT_POOL;
        },
        None => {
            *overlay = Some(Pool::new(
                OVERLAY_POOL,
                OVERLAY_TOP,
                OVERLAY_LEFT,
                OVERLAY_WIDTH,
                OVERLAY_HEIGHT,
                OVERLAY_BG,
                "OVL",
            ));
            *active = OVERLAY_POOL;

            if live {
                let anchor = ridden.then_some((LEFT_POOL, host_top));
                encode_reset_into(out);
                write_overlay_chrome(out, anchor);
            }
        },
    }
}

/// Draw the overlay's frame and title, either riding `anchor`'s pool or fixed
/// to the screen.
///
/// Both carry the same anchor as the overlay pool itself, so the whole note
/// travels as one piece rather than leaving its frame behind.
fn write_overlay_chrome(out: &mut Vec<u8>, anchor: Option<(u32, f32)>) {
    encode_panel_into(
        out,
        &PanelCommand {
            top: OVERLAY_TOP,
            left: OVERLAY_LEFT,
            width: OVERLAY_WIDTH,
            height: OVERLAY_HEIGHT,
            style: BorderStyle::Light,
            border: CHROME_FG,
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: true,
            anchor,
        },
    );
    encode_text_run_into(
        out,
        &TextRunCommand {
            col: (OVERLAY_LEFT as i16 + 2) * 16,
            row: OVERLAY_TOP as i16 * 16,
            scale: 160,
            color: CHROME_FG,
            bg: Some(OVERLAY_BG),
            follow: 0,
            anchor,
            text: " note ",
        },
    );
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
    let title = " stoatty multi-pool smooth scroll  (wheel, j/k, o, a, x, q) ";
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
    let footer = " the cursor and the overlay ride their pools; x and a withhold those anchors ";
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
