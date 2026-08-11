//! Composite a decoded APC scene into a snapshot buffer.
//!
//! Under stoatty, rich chrome -- frames, scaled text, bars, icons -- draws off
//! the cell grid as APC components rather than into the terminal buffer. So the
//! test harness can still snapshot and scan that rich rendering through the same
//! buffer it scans the cell fallback with, this reproduces each component's
//! cell-fallback layout on top of the rendered buffer.

use ratatui::{buffer::Buffer, style::Color};
use stoatty_protocol::command::{Command, IconKind};

/// Draw each decoded component's cell-fallback layout onto `buf`, in `cmds`
/// (paint) order.
///
/// Only components that carry visible text or chrome are reproduced. Scroll,
/// pool, minimap, and geometry state are skipped because their content already
/// lives in the rendered grid.
pub(crate) fn composite_scene(buf: &mut Buffer, cmds: &[Command]) {
    for cmd in cmds {
        match cmd {
            Command::TextRun(c) => {
                let x0 = (c.col / 16).max(0) as u16;
                let y = (c.row / 16).max(0) as u16;
                let fg = rgb(c.color);
                let bg = c.bg.map(rgb);
                for (i, ch) in c.text.chars().enumerate() {
                    set_cell(buf, x0 + i as u16, y, ch, fg, bg);
                }
            },
            Command::Panel(c) => {
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.border));
                if let Some(fill) = c.fill {
                    fill_interior(buf, c.left, c.top, c.width, c.height, rgb(fill));
                }
            },
            Command::Border(c) => {
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.color));
            },
            Command::Popover(c) => {
                fill_interior(buf, c.left, c.top, c.width, c.height, rgb(c.fill));
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.border));
                let fg = rgb(c.content_fg);
                for (i, ch) in c.content.chars().enumerate() {
                    set_cell(buf, c.left + 1 + i as u16, c.top + 1, ch, fg, None);
                }
            },
            Command::Bar(c) => {
                let x = (c.x / 16).max(0) as u16;
                let y = (c.y / 16).max(0) as u16;
                let w = (c.width / 16).max(1);
                let h = (c.height / 16).max(1);
                let color = rgb(c.color);
                if c.height < 16 {
                    for i in 0..w {
                        set_hairline(buf, x + i, y, '─', color);
                    }
                } else if c.width < 16 {
                    for i in 0..h {
                        set_hairline(buf, x, y + i, '│', color);
                    }
                } else {
                    for j in 0..h {
                        for i in 0..w {
                            set_bg(buf, x + i, y + j, color);
                        }
                    }
                }
            },
            Command::Icon(c) => {
                set_cell(buf, c.left, c.top, icon_sigil(c.kind), rgb(c.color), None);
            },
            _ => {},
        }
    }
}

fn rgb(color: [u8; 3]) -> Color {
    Color::Rgb(color[0], color[1], color[2])
}

fn icon_sigil(kind: IconKind) -> char {
    match kind {
        IconKind::Error => 'E',
        IconKind::Warning => 'W',
        IconKind::Info => 'I',
    }
}

fn set_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, fg: Color, bg: Option<Color>) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    let cell = &mut buf[(x, y)];
    cell.set_char(ch);
    cell.set_fg(fg);
    if let Some(bg) = bg {
        cell.set_bg(bg);
    }
}

fn set_bg(buf: &mut Buffer, x: u16, y: u16, bg: Color) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    buf[(x, y)].set_bg(bg);
}

/// Draw a hairline glyph only where the cell is blank.
///
/// A sub-cell bar (a separator, a gutter rule) would over-occlude a whole cell of
/// text if it always won, so it yields to any glyph already painted -- the text
/// underneath a separator stays readable in the composited snapshot.
fn set_hairline(buf: &mut Buffer, x: u16, y: u16, ch: char, fg: Color) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    let symbol = buf[(x, y)].symbol();
    if symbol.is_empty() || symbol == " " {
        let cell = &mut buf[(x, y)];
        cell.set_char(ch);
        cell.set_fg(fg);
    }
}

/// Draw a plain box perimeter, mirroring the widget frame fallbacks. Skips a box
/// too small to have a distinct border on every side.
fn draw_box(buf: &mut Buffer, left: u16, top: u16, width: u16, height: u16, fg: Color) {
    if width < 2 || height < 2 {
        return;
    }
    let right = left + width - 1;
    let bottom = top + height - 1;
    set_cell(buf, left, top, '┌', fg, None);
    set_cell(buf, right, top, '┐', fg, None);
    set_cell(buf, left, bottom, '└', fg, None);
    set_cell(buf, right, bottom, '┘', fg, None);
    for x in left + 1..right {
        set_cell(buf, x, top, '─', fg, None);
        set_cell(buf, x, bottom, '─', fg, None);
    }
    for y in top + 1..bottom {
        set_cell(buf, left, y, '│', fg, None);
        set_cell(buf, right, y, '│', fg, None);
    }
}

fn fill_interior(buf: &mut Buffer, left: u16, top: u16, width: u16, height: u16, bg: Color) {
    if width < 3 || height < 3 {
        return;
    }
    let area = buf.area;
    for y in top + 1..top + height - 1 {
        for x in left + 1..left + width - 1 {
            if x >= area.x && x < area.right() && y >= area.y && y < area.bottom() {
                buf[(x, y)].set_bg(bg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers::dispatch, Stoat};

    /// Open a fixed file, run `keys`, and return the last captured frame's text,
    /// composited from the APC scene the harness records.
    fn frame_text(keys: &str) -> String {
        let mut h = Stoat::test();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        std::mem::forget(rx);
        let path = std::path::PathBuf::from("/apc/a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        if !keys.is_empty() {
            h.type_keys(keys);
        }
        h.snapshot();
        h.rendered_text()
    }

    /// The which-key box rows, from its top-left corner to its bottom-left, so
    /// the box can be compared without the surrounding editor and bar.
    fn box_rows(text: &str) -> Vec<String> {
        let rows: Vec<String> = text.lines().map(str::to_string).collect();
        let top = rows.iter().position(|r| r.contains('┌'));
        let bottom = rows.iter().rposition(|r| r.contains('└'));
        match (top, bottom) {
            (Some(t), Some(b)) if b >= t => rows[t..=b].to_vec(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn composited_frame_paints_the_which_key_box() {
        let rows = box_rows(&frame_text("space"));
        assert!(
            rows.len() >= 2,
            "the composited frame paints a which-key box, got {rows:?}"
        );
    }

    #[test]
    fn composited_frame_shows_gutter_numbers_and_status_text() {
        let composited = frame_text("");
        let gutter: String = composited
            .lines()
            .flat_map(|row| row.chars().take(4))
            .filter(char::is_ascii_digit)
            .collect();
        assert!(
            gutter.contains('1') && gutter.contains('2'),
            "the gutter columns carry line numbers:\n{composited}"
        );
        assert!(
            composited.contains("a.txt"),
            "the composited status bar shows the filename:\n{composited}"
        );
    }

    #[test]
    fn apc_only_change_records_a_frame() {
        let mut h = Stoat::test();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        std::mem::forget(rx);
        let path = std::path::PathBuf::from("/apc/a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        h.snapshot();

        let before = h.frames().len();
        h.type_keys("space");
        let recorded = h.frames().len() > before;
        assert!(recorded, "an APC-only change records a new frame");
        assert!(
            h.frames().last().unwrap().content.contains("space"),
            "the recorded frame captures the which-key box title"
        );
    }
}
