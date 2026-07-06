use ratatui::{buffer::Buffer, style::Color};

/// A run of cells to re-stamp with a severity-colored curly underline.
///
/// `x`/`y` is the run's leftmost cell and `len` its width in cells, in terminal
/// coordinates. `color` is the undercurl color (the diagnostic severity),
/// carried separately from the cells' own foreground so the text keeps its
/// syntax coloring while only the decoration is added.
pub(crate) struct UndercurlSpan {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) len: u16,
    pub(crate) color: [u8; 3],
}

/// Build the raw VT that re-stamps `spans` over the already-painted `buf` with
/// curly underlines, or an empty vec when there are no spans.
///
/// ratatui's cell model cannot express undercurl, so the squiggle rides as raw
/// escapes over the grid ratatui drew. Each cell keeps its own foreground and
/// background and gains only SGR 4:3 undercurl plus the SGR 58 underline color;
/// consecutive cells sharing `(fg, bg)` collapse into one escape run so syntax
/// coloring is preserved with minimal output. The batch is wrapped in
/// DECSC/DECRC so the terminal cursor returns to wherever the grid draw left it.
pub(crate) fn build(buf: &Buffer, spans: &[UndercurlSpan]) -> Vec<u8> {
    if spans.is_empty() {
        return Vec::new();
    }

    let mut out = String::from("\x1b7");

    for span in spans {
        out.push_str(&format!("\x1b[{};{}H", span.y + 1, span.x + 1));

        let [ur, ug, ub] = span.color;
        let mut run_start = 0u16;
        while run_start < span.len {
            let (fg, bg) = cell_colors(buf, span.x + run_start, span.y);
            let mut run_end = run_start + 1;
            while run_end < span.len && cell_colors(buf, span.x + run_end, span.y) == (fg, bg) {
                run_end += 1;
            }

            out.push_str(&format!(
                "\x1b[0;{};{};4:3;58:2::{ur}:{ug}:{ub}m",
                sgr_fg(fg),
                sgr_bg(bg),
            ));
            for x in (span.x + run_start)..(span.x + run_end) {
                out.push_str(buf[(x, span.y)].symbol());
            }

            run_start = run_end;
        }
    }

    out.push_str("\x1b8\x1b[0m");
    out.into_bytes()
}

fn cell_colors(buf: &Buffer, x: u16, y: u16) -> (Color, Color) {
    let cell = &buf[(x, y)];
    (cell.fg, cell.bg)
}

fn sgr_fg(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        _ => "39".to_string(),
    }
}

fn sgr_bg(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        _ => "49".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{build, UndercurlSpan};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };

    fn paint(buf: &mut Buffer, x: u16, s: &str, fg: Color, bg: Color) {
        for (i, ch) in s.chars().enumerate() {
            buf[(x + i as u16, 0)]
                .set_char(ch)
                .set_style(Style::default().fg(fg).bg(bg));
        }
    }

    #[test]
    fn empty_spans_emit_nothing() {
        let buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        assert!(build(&buf, &[]).is_empty());
    }

    #[test]
    fn one_run_re_stamps_cells_with_the_curl() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(&mut buf, 2, "fn", Color::Rgb(1, 2, 3), Color::Rgb(4, 5, 6));
        let spans = [UndercurlSpan {
            x: 2,
            y: 0,
            len: 2,
            color: [7, 8, 9],
        }];
        let bytes = String::from_utf8(build(&buf, &spans)).unwrap();
        assert_eq!(
            bytes,
            "\x1b7\x1b[1;3H\x1b[0;38;2;1;2;3;48;2;4;5;6;4:3;58:2::7:8:9mfn\x1b8\x1b[0m"
        );
    }

    #[test]
    fn a_foreground_change_splits_into_two_runs() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(
            &mut buf,
            0,
            "ab",
            Color::Rgb(10, 10, 10),
            Color::Rgb(0, 0, 0),
        );
        paint(
            &mut buf,
            2,
            "c",
            Color::Rgb(20, 20, 20),
            Color::Rgb(0, 0, 0),
        );
        let spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 3,
            color: [1, 1, 1],
        }];
        let bytes = String::from_utf8(build(&buf, &spans)).unwrap();
        assert_eq!(
            bytes,
            "\x1b7\x1b[1;1H\
             \x1b[0;38;2;10;10;10;48;2;0;0;0;4:3;58:2::1:1:1mab\
             \x1b[0;38;2;20;20;20;48;2;0;0;0;4:3;58:2::1:1:1mc\
             \x1b8\x1b[0m"
        );
    }

    #[test]
    fn a_reset_colored_cell_uses_default_sgr() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf[(0, 0)].set_char('x');
        let spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 1,
            color: [2, 3, 4],
        }];
        let bytes = String::from_utf8(build(&buf, &spans)).unwrap();
        assert_eq!(
            bytes,
            "\x1b7\x1b[1;1H\x1b[0;39;49;4:3;58:2::2:3:4mx\x1b8\x1b[0m"
        );
    }
}
