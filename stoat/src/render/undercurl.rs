use ratatui::{buffer::Buffer, style::Color};
use std::fmt::{self, Display, Formatter, Write};

/// A run of cells to re-stamp with a severity-colored curly underline.
///
/// `x`/`y` is the run's leftmost cell and `len` its width in cells, in terminal
/// coordinates. `color` is the undercurl color (the diagnostic severity),
/// carried separately from the cells' own foreground so the text keeps its
/// syntax coloring while only the decoration is added.
///
/// `cells` records the span's buffer cells as the editor left them, before the
/// overlay stack painted. The re-stamp runs after every overlay, so [`build`]
/// drops any cell the final buffer no longer matches, meaning a cell a later
/// layer repainted, and the squiggle never lands on an overlay covering the
/// span. Populate it with [`snapshot_cells`] once editor painting is done.
pub(crate) struct UndercurlSpan {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) len: u16,
    pub(crate) color: [u8; 3],
    pub(crate) cells: Vec<ratatui::buffer::Cell>,
}

/// One frame's undercurl spans, retaining their cell records between frames.
///
/// A span's record is a vector sized to the run, and a frame that dropped the
/// spans would rebuild every one of those from scratch. Retiring a span by
/// moving a cursor instead keeps its allocation for the next frame's span to
/// refill.
///
/// The retired tail stays in the vector, so it must not be reachable: a span
/// left over from an earlier frame would re-stamp a squiggle over whatever now
/// occupies its cells. Only the live prefix is handed out.
#[derive(Default)]
pub(crate) struct UndercurlBatch {
    spans: Vec<UndercurlSpan>,
    live: usize,
}

impl UndercurlBatch {
    /// Retire the previous frame's spans, keeping their records' allocations.
    pub(crate) fn begin(&mut self) {
        self.live = 0;
    }

    /// Add a span over `len` cells from `(x, y)`, reusing a retired span's cell
    /// record when one is available.
    pub(crate) fn push(&mut self, x: u16, y: u16, len: u16, color: [u8; 3]) {
        match self.spans.get_mut(self.live) {
            Some(span) => {
                span.x = x;
                span.y = y;
                span.len = len;
                span.color = color;
                span.cells.clear();
            },
            None => self.spans.push(UndercurlSpan {
                x,
                y,
                len,
                color,
                cells: Vec::new(),
            }),
        }
        self.live += 1;
    }

    /// This frame's spans, without the retired tail.
    pub(crate) fn spans(&self) -> &[UndercurlSpan] {
        &self.spans[..self.live]
    }

    pub(crate) fn spans_mut(&mut self) -> &mut [UndercurlSpan] {
        &mut self.spans[..self.live]
    }
}

/// Record each span's current buffer cells so [`build`] can later drop any cell
/// a subsequently-painted overlay has overwritten.
///
/// Call once the editor panes have painted (cursor and selections included) and
/// before the overlay stack draws, so the record reflects the editor's output
/// and every later layer counts as a change.
///
/// Each record is replaced rather than appended to, so a span carrying one from
/// an earlier frame comes out sized to its current run.
pub(crate) fn snapshot_cells(buf: &Buffer, spans: &mut [UndercurlSpan]) {
    for span in spans {
        span.cells.clear();
        span.cells
            .extend((0..span.len).map(|i| buf[(span.x + i, span.y)].clone()));
    }
}

/// Build the raw VT that re-stamps `spans` over the already-painted `buf` with
/// curly underlines, or an empty vec when nothing is left to stamp.
///
/// ratatui's cell model cannot express undercurl, so the squiggle rides as raw
/// escapes over the grid ratatui drew. Each cell keeps its own foreground and
/// background and gains only SGR 4:3 undercurl plus the SGR 58 underline color.
/// Consecutive cells sharing `(fg, bg)` collapse into one escape run so syntax
/// coloring is preserved with minimal output.
///
/// A cell whose final buffer contents no longer match its `cells` record was
/// repainted by a later overlay, so it is skipped and the surviving cells split
/// into contiguous segments, each with its own cursor move. A span fully covered
/// by an overlay contributes nothing, and a frame with no surviving cell returns
/// an empty vec so no bare DECSC/DECRC pair is emitted. The batch is wrapped in
/// DECSC/DECRC so the terminal cursor returns to wherever the grid draw left it.
pub(crate) fn build(buf: &Buffer, spans: &[UndercurlSpan]) -> Vec<u8> {
    let mut body = String::new();

    for span in spans {
        let [ur, ug, ub] = span.color;
        let mut i = 0u16;
        while i < span.len {
            if !cell_survived(buf, span, i) {
                i += 1;
                continue;
            }
            let segment_start = i;
            while i < span.len && cell_survived(buf, span, i) {
                i += 1;
            }
            let segment_end = i;

            write!(body, "\x1b[{};{}H", span.y + 1, span.x + segment_start + 1)
                .expect("writing to a String is infallible");

            let mut run_start = segment_start;
            while run_start < segment_end {
                let (fg, bg) = cell_colors(buf, span.x + run_start, span.y);
                let mut run_end = run_start + 1;
                while run_end < segment_end
                    && cell_colors(buf, span.x + run_end, span.y) == (fg, bg)
                {
                    run_end += 1;
                }

                write!(
                    body,
                    "\x1b[0;{};{};4:3;58:2::{ur}:{ug}:{ub}m",
                    SgrFg(fg),
                    SgrBg(bg),
                )
                .expect("writing to a String is infallible");
                for x in (span.x + run_start)..(span.x + run_end) {
                    body.push_str(buf[(x, span.y)].symbol());
                }

                run_start = run_end;
            }
        }
    }

    if body.is_empty() {
        return Vec::new();
    }

    let mut out = String::from("\x1b7");
    out.push_str(&body);
    out.push_str("\x1b8\x1b[0m");
    out.into_bytes()
}

/// Whether the cell at offset `i` in `span` still matches its editor-paint
/// record, meaning no later overlay repainted it.
fn cell_survived(buf: &Buffer, span: &UndercurlSpan, i: u16) -> bool {
    span.cells
        .get(i as usize)
        .is_some_and(|recorded| buf[(span.x + i, span.y)] == *recorded)
}

fn cell_colors(buf: &Buffer, x: u16, y: u16) -> (Color, Color) {
    let cell = &buf[(x, y)];
    (cell.fg, cell.bg)
}

struct SgrFg(Color);

impl Display for SgrFg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            Color::Rgb(r, g, b) => write!(f, "38;2;{r};{g};{b}"),
            _ => f.write_str("39"),
        }
    }
}

struct SgrBg(Color);

impl Display for SgrBg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            Color::Rgb(r, g, b) => write!(f, "48;2;{r};{g};{b}"),
            _ => f.write_str("49"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build, snapshot_cells, UndercurlBatch, UndercurlSpan};
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

    /// Retiring a span is what makes its cell record available to the next
    /// frame, and the record is the whole reason the batch outlives a frame.
    /// Retired spans also have to be out of reach, since a re-stamp would draw
    /// their squiggle over whatever now occupies those cells.
    #[test]
    fn a_retired_span_lends_its_cell_record_to_the_next_frame() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(&mut buf, 0, "abc", Color::Rgb(1, 1, 1), Color::Rgb(0, 0, 0));

        let mut batch = UndercurlBatch::default();
        batch.begin();
        batch.push(0, 0, 3, [9, 9, 9]);
        snapshot_cells(&buf, batch.spans_mut());

        let record = batch.spans()[0].cells.capacity();
        assert!(record >= 3, "the run's cells were recorded, got {record}");

        batch.begin();
        assert!(batch.spans().is_empty(), "the retired span is out of reach");

        batch.push(4, 0, 2, [1, 2, 3]);
        let reused = &batch.spans()[0];
        assert_eq!(
            (reused.x, reused.len, reused.color, reused.cells.capacity()),
            (4, 2, [1, 2, 3], record),
            "the new span overwrote the retired one and kept its record",
        );
        assert!(reused.cells.is_empty(), "with nothing recorded yet");
    }

    /// A record is sized to its span's run, so recording into one that already
    /// holds cells has to replace them. Appending would leave the record longer
    /// than the run it stands for.
    #[test]
    fn recording_replaces_a_record_rather_than_growing_it() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(&mut buf, 0, "abc", Color::Rgb(1, 1, 1), Color::Rgb(0, 0, 0));

        let mut spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 3,
            color: [9, 9, 9],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
        snapshot_cells(&buf, &mut spans);

        assert_eq!(spans[0].cells.len(), 3, "one cell per column of the run");
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
        let mut spans = [UndercurlSpan {
            x: 2,
            y: 0,
            len: 2,
            color: [7, 8, 9],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
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
        let mut spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 3,
            color: [1, 1, 1],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
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
        let mut spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 1,
            color: [2, 3, 4],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
        let bytes = String::from_utf8(build(&buf, &spans)).unwrap();
        assert_eq!(
            bytes,
            "\x1b7\x1b[1;1H\x1b[0;39;49;4:3;58:2::2:3:4mx\x1b8\x1b[0m"
        );
    }

    #[test]
    fn an_overlay_over_the_span_middle_splits_into_two_segments() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(&mut buf, 0, "abc", Color::Rgb(1, 1, 1), Color::Rgb(0, 0, 0));
        let mut spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 3,
            color: [9, 9, 9],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
        // An overlay repaints the middle cell after the snapshot.
        paint(&mut buf, 1, "X", Color::Rgb(2, 2, 2), Color::Rgb(3, 3, 3));

        let bytes = String::from_utf8(build(&buf, &spans)).unwrap();
        assert_eq!(
            bytes,
            "\x1b7\
             \x1b[1;1H\x1b[0;38;2;1;1;1;48;2;0;0;0;4:3;58:2::9:9:9ma\
             \x1b[1;3H\x1b[0;38;2;1;1;1;48;2;0;0;0;4:3;58:2::9:9:9mc\
             \x1b8\x1b[0m"
        );
    }

    #[test]
    fn an_overlay_over_the_whole_span_emits_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        paint(&mut buf, 0, "ab", Color::Rgb(1, 1, 1), Color::Rgb(0, 0, 0));
        let mut spans = [UndercurlSpan {
            x: 0,
            y: 0,
            len: 2,
            color: [9, 9, 9],
            cells: Vec::new(),
        }];
        snapshot_cells(&buf, &mut spans);
        // An overlay repaints every span cell after the snapshot.
        paint(&mut buf, 0, "XY", Color::Rgb(2, 2, 2), Color::Rgb(3, 3, 3));

        assert!(build(&buf, &spans).is_empty());
    }
}
