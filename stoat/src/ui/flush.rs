//! How a rendered frame reaches the terminal.
//!
//! ratatui flushes a frame by copying it into a back buffer of its own and
//! diffing that against the buffer it flushed before, cell by cell over the
//! whole grid. The app publishes a frame it already painted, so the copy buys
//! nothing. In stoatty, where the pools hold the content, a cursor move changes
//! a single row of the grid. [`ShownFrame`] keeps the last flushed frame and
//! diffs the next one against it a row at a time, so a row that did not change
//! costs one comparison and is never copied.

use ratatui::{
    backend::Backend,
    buffer::{Buffer, Cell, CellDiffOption, CellWidth},
    layout::Rect,
    style::{Color, Modifier},
    Terminal,
};

/// Modifiers that paint a blank cell, so a wide cell that carried one leaves it
/// on its trailing columns until they are written again.
const VISIBLE_ON_BLANK: Modifier = Modifier::REVERSED
    .union(Modifier::UNDERLINED)
    .union(Modifier::SLOW_BLINK)
    .union(Modifier::RAPID_BLINK)
    .union(Modifier::CROSSED_OUT);

/// The frame the terminal shows, as the cells last flushed to it.
///
/// A copy of its own rather than the published frame. The run loop takes each
/// published buffer back once the UI thread drops it, and blanks it off the
/// loop for a later paint. A buffer the UI thread still holds comes back
/// shared, and the paint that reuses it blanks it on the loop.
#[derive(Default)]
pub(super) struct ShownFrame {
    /// `None` until a frame reaches the screen whole, and again after anything
    /// leaves the screen in a state no frame describes.
    buffer: Option<Buffer>,
}

impl ShownFrame {
    /// Bring the screen from the shown frame to `frame`, with the terminal
    /// cursor at `cursor`, or hidden when it is `None`.
    ///
    /// A frame of the shown frame's size, on a terminal of that size, writes
    /// only the cells that differ, as ratatui's own diff picks them. Any other
    /// frame clears the screen and writes every cell, clamped to the terminal.
    pub(super) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        frame: &Buffer,
        cursor: Option<(u16, u16)>,
    ) -> Result<(), B::Error> {
        let screen = Rect::from(terminal.size()?);
        if let Some(shown) = self.buffer.as_mut()
            && shown.area == frame.area
            && shown.area == screen
        {
            let mut changed = Vec::new();
            terminal
                .backend_mut()
                .draw(FrameDiff::new(shown, frame, &mut changed))?;
            // Through the terminal rather than its backend, because the terminal
            // records a hidden cursor and shows it again when it drops.
            match cursor {
                Some(position) => {
                    terminal.show_cursor()?;
                    terminal.set_cursor_position(position)?;
                },
                None => terminal.hide_cursor()?,
            }
            terminal.backend_mut().flush()?;

            let width = usize::from(frame.area.width);
            for row in changed {
                let cells = row * width..(row + 1) * width;
                shown.content[cells.clone()].clone_from_slice(&frame.content[cells]);
            }
            return Ok(());
        }

        self.repaint_all(terminal)?;
        terminal.draw(|f| {
            let dst = f.buffer_mut();
            if dst.area == frame.area {
                dst.content.clone_from(&frame.content);
            } else {
                copy_clamped(dst, frame);
            }
            if let Some(position) = cursor {
                f.set_cursor_position(position);
            }
        })?;
        self.buffer = (frame.area == screen).then(|| frame.clone());
        Ok(())
    }

    /// Clear the screen and have the next frame paint every cell.
    ///
    /// A resize to the size the terminal already has does both, without the
    /// cursor-position query that [`Terminal::clear`] sends to restore the
    /// cursor. The query goes out while the input thread runs its handshake
    /// against a terminal that just attached. The two readers of fd 0 then take
    /// each other's replies. A terminal that answers no query makes the clear
    /// time out, and the error ends the UI thread.
    pub(super) fn repaint_all<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<(), B::Error> {
        self.buffer = None;
        let size = terminal.size()?;
        terminal.resize(size.into())
    }
}

/// The cell writes that bring a screen showing one frame to the next, a row at
/// a time.
///
/// A row whose cells all equal the shown row writes nothing. A changed row
/// writes by the rules of ratatui's own diff,
/// [`BufferDiff`](ratatui::buffer::BufferDiff), copied here because that diff
/// walks only whole buffers. The rules write a cell that differs, step over the
/// columns a wide cell covers, honor each cell's [`CellDiffOption`], and write
/// again the trailing columns where some terminals leave stale paint.
///
/// Each row starts the rules afresh, and a span stops at its row's end.
/// ratatui's walk carries a span past a row's end into the next row's first
/// columns, where no terminal draws it.
struct FrameDiff<'a, 'frame> {
    shown: &'a [Cell],
    frame: &'frame [Cell],
    area: Rect,
    /// The next row to compare.
    row: usize,
    /// The next cell the rules visit in the open row.
    pos: usize,
    /// Where the open row ends.
    end: usize,
    trailing: Option<Trailing>,
    /// Each row that held a change, for the caller to copy once the writes
    /// are out.
    changed: &'a mut Vec<usize>,
}

/// The trailing columns of a written wide cell that the rules still visit.
struct Trailing {
    next_index: usize,
    end: usize,
    /// Write every visited column, whether its symbol changed or not.
    force: bool,
}

impl<'a, 'frame> FrameDiff<'a, 'frame> {
    fn new(shown: &'a Buffer, frame: &'frame Buffer, changed: &'a mut Vec<usize>) -> Self {
        Self {
            shown: &shown.content,
            frame: &frame.content,
            area: frame.area,
            row: 0,
            pos: 0,
            end: 0,
            trailing: None,
            changed,
        }
    }

    /// Open the next row that differs from the shown row, or report that no
    /// row is left.
    fn open_changed_row(&mut self) -> bool {
        let width = usize::from(self.area.width);
        while self.row < usize::from(self.area.height) {
            let row = self.row;
            self.row += 1;

            let (start, end) = (row * width, (row + 1) * width);
            if !row_unchanged(&self.shown[start..end], &self.frame[start..end]) {
                self.changed.push(row);
                self.pos = start;
                self.end = end;
                return true;
            }
        }
        false
    }

    /// The next write in the open row.
    fn next_in_row(&mut self) -> Option<(u16, u16, &'frame Cell)> {
        if let Some(Trailing {
            next_index,
            end,
            force,
        }) = &mut self.trailing
        {
            while *next_index < *end {
                let j = *next_index;
                // A wide cell in the trailing columns covers columns of its
                // own, which the walk steps over rather than writing blank.
                *next_index += usize::from(self.frame[j].cell_width().max(1));
                *end = (*end).max(*next_index).min(self.end);

                if !is_skip(&self.frame[j])
                    && (*force || self.shown[j].symbol() != self.frame[j].symbol())
                {
                    return Some(self.write(j));
                }
            }
            self.pos = *end;
            self.trailing = None;
        }

        while self.pos < self.end {
            let i = self.pos;
            self.pos += 1;
            let current = &self.frame[i];
            let previous = &self.shown[i];

            match current.diff_option {
                CellDiffOption::Skip => {},
                _ if is_skip(current) => {},
                CellDiffOption::ForcedWidth(width) => {
                    self.pos = self
                        .pos
                        .saturating_add(usize::from(width.get()).saturating_sub(1));
                    if current != previous {
                        return Some(self.write(i));
                    }
                },
                CellDiffOption::None | CellDiffOption::AlwaysUpdate => {
                    let width = usize::from(current.cell_width());
                    if current.diff_option == CellDiffOption::None && current == previous {
                        self.pos += width.saturating_sub(1);
                        continue;
                    }

                    let previous_width = usize::from(previous.cell_width());
                    // Some terminals leave the trailing column of an emoji
                    // presentation sequence uncleared. Other wide cells skip
                    // their trailing columns, which terminals clear themselves.
                    let vs16 = width > 1 && current.symbol().contains('\u{fe0f}');
                    if vs16 {
                        self.trailing = Some(Trailing {
                            next_index: i + 1,
                            end: (i + width).min(self.end),
                            force: false,
                        });
                    } else if width > 1 {
                        self.pos += width - 1;
                    } else if previous_width > width
                        && (previous.bg != Color::Reset
                            || previous.modifier.intersects(VISIBLE_ON_BLANK))
                    {
                        // The wide cell this one replaces painted its trailing
                        // columns, which the terminal keeps showing until they
                        // are written again.
                        self.trailing = Some(Trailing {
                            next_index: i + 1,
                            end: (i + previous_width).min(self.end),
                            force: true,
                        });
                    }
                    return Some(self.write(i));
                },
            }
        }
        None
    }

    fn write(&self, index: usize) -> (u16, u16, &'frame Cell) {
        let width = usize::from(self.area.width);
        let x = index % width + usize::from(self.area.x);
        let y = index / width + usize::from(self.area.y);
        (x as u16, y as u16, &self.frame[index])
    }
}

impl<'frame> Iterator for FrameDiff<'_, 'frame> {
    type Item = (u16, u16, &'frame Cell);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(write) = self.next_in_row() {
                return Some(write);
            }
            if !self.open_changed_row() {
                return None;
            }
        }
    }
}

/// Whether a row writes nothing, because every cell equals the shown one and
/// none asks to be written whatever it holds.
fn row_unchanged(shown: &[Cell], frame: &[Cell]) -> bool {
    shown
        .iter()
        .zip(frame)
        .all(|(shown, cell)| shown == cell && cell.diff_option != CellDiffOption::AlwaysUpdate)
}

/// Whether the rules leave `cell` alone.
#[allow(deprecated)] // ratatui's own diff still honors the deprecated skip flag
fn is_skip(cell: &Cell) -> bool {
    cell.diff_option == CellDiffOption::Skip
        || (cell.skip && cell.diff_option == CellDiffOption::None)
}

/// Copy `src` into `dst` over their overlapping top-left region, leaving `dst`'s
/// area unchanged.
///
/// `dst` is the terminal's buffer, which ratatui's autoresize holds at the live
/// terminal size. `src` is the frame the main thread rendered, which lags a
/// resize by a frame and then still carries the previous size. Resizing `dst`
/// to match is never an option, because ratatui flushes by diffing `dst`
/// against its sibling buffer at the live size, and a differing origin or width
/// there panics. Copying only the intersection keeps that diff valid, and an
/// uncovered margin stays blank for the one frame until the main thread paints
/// at the new size.
fn copy_clamped(dst: &mut Buffer, src: &Buffer) {
    dst.reset();

    let cols = dst.area.width.min(src.area.width);
    let rows = dst.area.height.min(src.area.height);
    for y in 0..rows {
        for x in 0..cols {
            dst[(x, y)] = src[(x, y)].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::{ClearType, TestBackend, WindowSize},
        layout::{Position, Size},
    };
    use std::{convert::Infallible, mem, num::NonZeroU16};

    /// One call a flush made on the terminal.
    #[derive(Debug, PartialEq)]
    enum Op {
        Draw(Vec<(u16, u16, String)>),
        ShowCursor,
        HideCursor,
        MoveCursor(u16, u16),
        QueryCursor,
        Clear(ClearType),
        Flush,
    }

    /// A terminal that records each call made on it and keeps the screen those
    /// calls leave.
    struct Recorder {
        screen: TestBackend,
        ops: Vec<Op>,
    }

    impl Backend for Recorder {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let cells: Vec<_> = content.collect();
            self.ops.push(Op::Draw(
                cells
                    .iter()
                    .map(|&(x, y, cell)| (x, y, cell.symbol().to_owned()))
                    .collect(),
            ));
            self.screen.draw(cells.into_iter())
        }

        fn hide_cursor(&mut self) -> Result<(), Infallible> {
            self.ops.push(Op::HideCursor);
            self.screen.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Infallible> {
            self.ops.push(Op::ShowCursor);
            self.screen.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Infallible> {
            self.ops.push(Op::QueryCursor);
            self.screen.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Infallible> {
            let position = position.into();
            self.ops.push(Op::MoveCursor(position.x, position.y));
            self.screen.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Infallible> {
            self.ops.push(Op::Clear(ClearType::All));
            self.screen.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Infallible> {
            self.ops.push(Op::Clear(clear_type));
            self.screen.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Infallible> {
            self.screen.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Infallible> {
            self.screen.window_size()
        }

        fn flush(&mut self) -> Result<(), Infallible> {
            self.ops.push(Op::Flush);
            self.screen.flush()
        }
    }

    /// A terminal `width` by `height` that has recorded no call yet.
    fn recorder(width: u16, height: u16) -> Terminal<Recorder> {
        let mut terminal = Terminal::new(Recorder {
            screen: TestBackend::new(width, height),
            ops: Vec::new(),
        })
        .unwrap();
        terminal.backend_mut().ops.clear();
        terminal
    }

    /// Flush a frame of `lines` and return the calls the flush made.
    fn flush(
        shown: &mut ShownFrame,
        terminal: &mut Terminal<Recorder>,
        lines: &[&str],
        cursor: Option<(u16, u16)>,
    ) -> Vec<Op> {
        let frame = Buffer::with_lines(lines.iter().copied());
        shown.flush(terminal, &frame, cursor).unwrap();
        mem::take(&mut terminal.backend_mut().ops)
    }

    fn draw(cells: &[(u16, u16, &str)]) -> Op {
        Op::Draw(
            cells
                .iter()
                .map(|&(x, y, symbol)| (x, y, symbol.to_owned()))
                .collect(),
        )
    }

    /// A frame writes the cells that differ from the frame flushed before it,
    /// and the cursor, and nothing of the rows it left alone.
    #[test]
    fn a_frame_writes_only_what_differs_from_the_frame_before() {
        let mut terminal = recorder(3, 2);
        let mut shown = ShownFrame::default();
        flush(&mut shown, &mut terminal, &["abc", "def"], None);

        assert_eq!(
            flush(&mut shown, &mut terminal, &["abc", "dxf"], Some((1, 1))),
            [
                draw(&[(1, 1, "x")]),
                Op::ShowCursor,
                Op::MoveCursor(1, 1),
                Op::Flush
            ],
            "one changed cell",
        );
        assert_eq!(
            flush(&mut shown, &mut terminal, &["abz", "dxf"], None),
            [draw(&[(2, 0, "z")]), Op::HideCursor, Op::Flush],
            "the second row holds what the frame before wrote",
        );
        terminal
            .backend()
            .screen
            .assert_buffer_lines(["abz", "dxf"]);
    }

    /// A terminal that changed size shows a screen no frame describes, so the
    /// next frame clears it and writes every cell, without asking the terminal
    /// where its cursor is.
    #[test]
    fn a_resize_between_frames_repaints_every_cell() {
        let mut terminal = recorder(3, 2);
        let mut shown = ShownFrame::default();
        flush(&mut shown, &mut terminal, &["abc", "def"], None);
        terminal.backend_mut().screen.resize(4, 2);

        assert_eq!(
            flush(&mut shown, &mut terminal, &["abcd", "defg"], None),
            [
                Op::Clear(ClearType::All),
                draw(&[
                    (0, 0, "a"),
                    (1, 0, "b"),
                    (2, 0, "c"),
                    (3, 0, "d"),
                    (0, 1, "d"),
                    (1, 1, "e"),
                    (2, 1, "f"),
                    (3, 1, "g"),
                ]),
                Op::HideCursor,
                Op::Flush,
            ],
        );
    }

    /// A frame painted before the terminal shrank draws clamped to the
    /// terminal, and repaints whole each time, since the terminal shows only
    /// part of it. Once the terminal grows back, the frame repaints whole again,
    /// because the part the terminal never showed was never written.
    #[test]
    fn a_frame_larger_than_the_terminal_draws_clamped_until_it_fits() {
        let mut terminal = recorder(3, 2);
        let mut shown = ShownFrame::default();
        flush(&mut shown, &mut terminal, &["abc", "def"], None);
        terminal.backend_mut().screen.resize(2, 1);
        let clamped = [
            Op::Clear(ClearType::All),
            draw(&[(0, 0, "a"), (1, 0, "b")]),
            Op::HideCursor,
            Op::Flush,
        ];

        assert_eq!(
            flush(&mut shown, &mut terminal, &["abc", "dxf"], None),
            [
                Op::Clear(ClearType::All),
                Op::Clear(ClearType::All),
                draw(&[(0, 0, "a"), (1, 0, "b")]),
                Op::HideCursor,
                Op::Flush,
            ],
            "first frame after the shrink, which ratatui clears once more for",
        );
        assert_eq!(
            flush(&mut shown, &mut terminal, &["abc", "dxf"], None),
            clamped,
            "second frame after the shrink",
        );

        terminal.backend_mut().screen.resize(3, 2);
        assert_eq!(
            flush(&mut shown, &mut terminal, &["abc", "dxf"], None),
            [
                Op::Clear(ClearType::All),
                draw(&[
                    (0, 0, "a"),
                    (1, 0, "b"),
                    (2, 0, "c"),
                    (0, 1, "d"),
                    (1, 1, "x"),
                    (2, 1, "f"),
                ]),
                Op::HideCursor,
                Op::Flush,
            ],
            "the frame after the terminal grows back",
        );
    }

    /// A frame smaller than the terminal repaints whole, over a screen that
    /// frames written a row at a time left behind.
    #[test]
    fn a_frame_smaller_than_the_terminal_repaints_the_whole_screen() {
        let mut terminal = recorder(3, 2);
        let mut shown = ShownFrame::default();
        flush(&mut shown, &mut terminal, &["abc", "def"], None);
        flush(&mut shown, &mut terminal, &["xbc", "def"], None);

        flush(&mut shown, &mut terminal, &["ab"], None);

        terminal
            .backend()
            .screen
            .assert_buffer_lines(["ab ", "   "]);
    }

    /// The row diff writes what ratatui's whole-grid diff writes, over random
    /// pairs of frames.
    #[test]
    fn the_row_diff_writes_what_the_grid_diff_writes() {
        let mut state = 1;
        for case in 0..4000 {
            let (shown, frame) = random_pair(&mut state);
            let mut changed = Vec::new();
            let rows: Vec<_> = FrameDiff::new(&shown, &frame, &mut changed).collect();
            assert_eq!(
                rows,
                shown.diff_iter(&frame).collect::<Vec<_>>(),
                "case {case}"
            );
        }
    }

    /// Two frames over one random area, the second a copy of the first with a
    /// few cells redrawn, or redrawn whole.
    fn random_pair(state: &mut u64) -> (Buffer, Buffer) {
        let area = Rect::new(
            (lcg(state) % 3) as u16,
            (lcg(state) % 3) as u16,
            1 + (lcg(state) % 8) as u16,
            1 + (lcg(state) % 4) as u16,
        );
        let random_frame = |state: &mut u64| {
            let mut frame = Buffer::empty(area);
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    frame[(x, y)] = random_cell(state, area.right() - x);
                }
            }
            frame
        };

        let shown = random_frame(state);
        let mut frame = shown.clone();
        if lcg(state).is_multiple_of(5) {
            frame = random_frame(state);
        }
        for _ in 0..lcg(state) % 4 {
            let x = area.left() + (lcg(state) % u64::from(area.width)) as u16;
            let y = area.top() + (lcg(state) % u64::from(area.height)) as u16;
            frame[(x, y)] = random_cell(state, area.right() - x);
        }
        (shown, frame)
    }

    /// A random cell for a column with `room` columns left in its row.
    ///
    /// Never one whose span runs past its row's end, where the row diff and
    /// ratatui's walk part ways by design.
    #[allow(deprecated)] // sets the deprecated skip flag, which the rules honor
    fn random_cell(state: &mut u64, room: u16) -> Cell {
        const SYMBOLS: [&str; 5] = ["a", "b", " ", "\u{4e16}", "\u{2764}\u{fe0f}"];
        let symbol = SYMBOLS[(lcg(state) % 5) as usize];
        let mut cell = Cell::new(if symbol.cell_width() <= room {
            symbol
        } else {
            "a"
        });
        if lcg(state).is_multiple_of(3) {
            cell.bg = Color::Indexed(1);
        }
        if lcg(state).is_multiple_of(4) {
            cell.modifier = Modifier::REVERSED;
        }
        cell.diff_option = match lcg(state) % 12 {
            0 => CellDiffOption::Skip,
            1 => CellDiffOption::AlwaysUpdate,
            2 if room >= 2 => CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap()),
            3 => CellDiffOption::ForcedWidth(NonZeroU16::MIN),
            _ => CellDiffOption::None,
        };
        cell.skip = lcg(state).is_multiple_of(16);
        cell
    }

    /// A deterministic pseudo-random walk, so the frames above cover shapes
    /// nobody hand-picked while staying reproducible.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    #[test]
    fn copy_clamped_keeps_dst_area_and_copies_overlap_when_src_is_larger() {
        let src = Buffer::with_lines(["abc", "def"]);
        let mut dst = Buffer::with_lines(["ZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["ab"]));
    }

    #[test]
    fn copy_clamped_clears_stale_margin_when_src_is_smaller() {
        let src = Buffer::with_lines(["xy"]);
        let mut dst = Buffer::with_lines(["ZZZ", "ZZZ"]);

        copy_clamped(&mut dst, &src);

        assert_eq!(dst, Buffer::with_lines(["xy ", "   "]));
    }
}
