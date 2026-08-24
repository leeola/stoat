//! Composite a decoded APC scene into a snapshot buffer.
//!
//! Under stoatty, rich chrome -- frames, scaled text, bars, icons -- draws off
//! the cell grid as APC components rather than into the terminal buffer. So the
//! test harness can still snapshot and scan that rich rendering through the same
//! buffer it scans the cell fallback with, this reproduces each component's
//! cell-fallback layout on top of the rendered buffer.

use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use stoatty_protocol::command::{Command, IconKind};

/// Draw each decoded component's cell-fallback layout onto `buf`, in `cmds`
/// (paint) order.
///
/// Only components that carry visible text or chrome are reproduced. Scroll,
/// pool, minimap, and geometry state are skipped because their content already
/// lives in the rendered grid.
///
/// A component never writes into the interior of a box painted after it. The
/// scene replays over the *finished* main pass, so without that rule an earlier
/// frame's border lands on cells a later widget already cleared and wrote,
/// inverting the paint order the real renderer has.
pub(crate) fn composite_scene(buf: &mut Buffer, cmds: &[Command]) {
    let boxes = box_interiors(cmds);
    for (idx, cmd) in cmds.iter().enumerate() {
        let above = &boxes[boxes.partition_point(|(i, _)| *i <= idx)..];
        match cmd {
            Command::TextRun(c) => {
                let x0 = (c.col / 16).max(0) as u16;
                let y = (c.row / 16).max(0) as u16;
                let fg = rgb(c.color);
                let bg = c.bg.map(rgb);
                for (i, ch) in c.text.chars().enumerate() {
                    set_cell(buf, x0 + i as u16, y, ch, fg, bg, above);
                }
            },
            Command::Panel(c) => {
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.border), above);
                if let Some(fill) = c.fill {
                    fill_interior(buf, c.left, c.top, c.width, c.height, rgb(fill), above);
                }
            },
            Command::Border(c) => {
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.color), above);
            },
            Command::Popover(c) => {
                fill_interior(buf, c.left, c.top, c.width, c.height, rgb(c.fill), above);
                draw_box(buf, c.left, c.top, c.width, c.height, rgb(c.border), above);
                let fg = rgb(c.content_fg);
                for (i, ch) in c.content.chars().enumerate() {
                    set_cell(buf, c.left + 1 + i as u16, c.top + 1, ch, fg, None, above);
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
                        set_hairline(buf, x + i, y, '─', color, above);
                    }
                } else if c.width < 16 {
                    for i in 0..h {
                        set_hairline(buf, x, y + i, '│', color, above);
                    }
                } else {
                    for j in 0..h {
                        for i in 0..w {
                            set_bg(buf, x + i, y + j, color, above);
                        }
                    }
                }
            },
            Command::Icon(c) => {
                set_cell(
                    buf,
                    c.left,
                    c.top,
                    icon_sigil(c.kind),
                    rgb(c.color),
                    None,
                    above,
                );
            },
            _ => {},
        }
    }
}

/// Each box component's paint index paired with the cells it owns, ordered by
/// index so a caller can slice off everything painted after it.
///
/// The interior excludes the border, which the box itself draws, and is empty
/// for a box too small to have one.
fn box_interiors(cmds: &[Command]) -> Vec<(usize, Rect)> {
    cmds.iter()
        .enumerate()
        .filter_map(|(idx, cmd)| {
            let (left, top, width, height) = match cmd {
                Command::Panel(c) => (c.left, c.top, c.width, c.height),
                Command::Border(c) => (c.left, c.top, c.width, c.height),
                Command::Popover(c) => (c.left, c.top, c.width, c.height),
                _ => return None,
            };
            if width < 3 || height < 3 {
                return None;
            }
            Some((idx, Rect::new(left + 1, top + 1, width - 2, height - 2)))
        })
        .collect()
}

fn owned_above(x: u16, y: u16, above: &[(usize, Rect)]) -> bool {
    above
        .iter()
        .any(|(_, r)| x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
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

fn set_cell(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    ch: char,
    fg: Color,
    bg: Option<Color>,
    above: &[(usize, Rect)],
) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    if owned_above(x, y, above) {
        return;
    }
    let cell = &mut buf[(x, y)];
    cell.set_char(ch);
    cell.set_fg(fg);
    if let Some(bg) = bg {
        cell.set_bg(bg);
    }
}

fn set_bg(buf: &mut Buffer, x: u16, y: u16, bg: Color, above: &[(usize, Rect)]) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    if owned_above(x, y, above) {
        return;
    }
    buf[(x, y)].set_bg(bg);
}

/// Draw a hairline glyph only where the cell is blank.
///
/// A sub-cell bar (a separator, a gutter rule) would over-occlude a whole cell of
/// text if it always won, so it yields to any glyph already painted -- the text
/// underneath a separator stays readable in the composited snapshot.
fn set_hairline(buf: &mut Buffer, x: u16, y: u16, ch: char, fg: Color, above: &[(usize, Rect)]) {
    let area = buf.area;
    if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    if owned_above(x, y, above) {
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
fn draw_box(
    buf: &mut Buffer,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    fg: Color,
    above: &[(usize, Rect)],
) {
    if width < 2 || height < 2 {
        return;
    }
    let right = left + width - 1;
    let bottom = top + height - 1;
    set_cell(buf, left, top, '┌', fg, None, above);
    set_cell(buf, right, top, '┐', fg, None, above);
    set_cell(buf, left, bottom, '└', fg, None, above);
    set_cell(buf, right, bottom, '┘', fg, None, above);
    for x in left + 1..right {
        set_cell(buf, x, top, '─', fg, None, above);
        set_cell(buf, x, bottom, '─', fg, None, above);
    }
    for y in top + 1..bottom {
        set_cell(buf, left, y, '│', fg, None, above);
        set_cell(buf, right, y, '│', fg, None, above);
    }
}

fn fill_interior(
    buf: &mut Buffer,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    bg: Color,
    above: &[(usize, Rect)],
) {
    if width < 3 || height < 3 {
        return;
    }
    for y in top + 1..top + height - 1 {
        for x in left + 1..left + width - 1 {
            set_bg(buf, x, y, bg, above);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::composite_scene;
    use crate::{action_handlers::dispatch, Stoat};
    use ratatui::{buffer::Buffer, layout::Rect};
    use stoatty_protocol::command::{BorderStyle, Command, PanelCommand, PanelShadow};

    /// The compositor replays every component over the finished main pass, so
    /// without the occlusion rule an earlier frame's border lands on cells the
    /// later widget already owns. Two overlapping boxes is the shape that
    /// exposes it: every modal draws its hints box over itself.
    #[test]
    fn a_later_panel_owns_its_interior_against_an_earlier_border() {
        let panel = |left, top, width, height| {
            Command::Panel(PanelCommand {
                top,
                left,
                width,
                height,
                style: BorderStyle::Rounded,
                border: [1, 2, 3],
                corner_radius: 0,
                fill: None,
                shadow: PanelShadow::None_,
                inset_x: 0,
                above_pools: false,
                anchor: None,
            })
        };

        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        composite_scene(&mut buf, &[panel(0, 0, 10, 4), panel(4, 1, 8, 5)]);

        assert_eq!(
            rows(&buf),
            [
                "┌────────┐  ",
                "│   ┌──────┐",
                "│   │      │",
                "└───│      │",
                "    │      │",
                "    └──────┘",
            ],
            "the earlier box stops at the later box's edge"
        );
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (buf.area.top()..buf.area.bottom())
            .map(|y| {
                (buf.area.left()..buf.area.right())
                    .map(|x| {
                        let symbol = buf[(x, y)].symbol();
                        if symbol.is_empty() {
                            " "
                        } else {
                            symbol
                        }
                    })
                    .collect()
            })
            .collect()
    }

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
