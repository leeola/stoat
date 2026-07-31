use crate::{
    sum_tree::{self, ContextLessSummary, Dimension},
    Bias, Dimensions, Item, OffsetUtf16, Point, PointUtf16, SumTree,
};
use arrayvec::ArrayString;
use regex_cursor::{Cursor as RegexCursor, Input};
use std::{cmp, ops::Range};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

#[cfg(not(test))]
type Bitmap = u128;
#[cfg(test)]
type Bitmap = u16;

const MAX_BASE: usize = Bitmap::BITS as usize;

/// Smallest chunk [`Rope::append`] leaves at a seam it controls.
///
/// Half a chunk is where merging starts paying and stays safe. Two chunks each
/// below it fit in one, so the merge never has to split again.
const MIN_BASE: usize = MAX_BASE / 2;

/// A rope's shape, summed from its chunks.
///
/// Equality is over the whole shape, which is a property of the text rather
/// than of how it happens to be chunked. Two ropes holding the same bytes
/// compare equal however they were built, which is what lets a caller use
/// inequality as proof that two ropes differ without reading either.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TextSummary {
    pub len: usize,
    pub len_utf16: OffsetUtf16,
    pub lines: Point,
    pub lines_utf16: PointUtf16,
    pub chars: usize,
    pub first_line_chars: u32,
    pub last_line_chars: u32,
    pub longest_row: u32,
    pub longest_row_chars: u32,
}

impl TextSummary {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        let mut lines = Point::zero();
        let mut len_utf16 = OffsetUtf16(0);
        let mut chars = 0usize;
        let mut current_line_chars = 0u32;
        let mut first_line_chars = 0u32;
        let mut longest_row = 0u32;
        let mut longest_row_chars = 0u32;
        let mut first_line_done = false;
        let mut lines_utf16_column = 0u32;

        for ch in text.chars() {
            len_utf16.0 += ch.len_utf16();
            chars += 1;

            if ch == '\n' {
                if !first_line_done {
                    first_line_chars = current_line_chars;
                    first_line_done = true;
                }
                if current_line_chars > longest_row_chars {
                    longest_row = lines.row;
                    longest_row_chars = current_line_chars;
                }
                lines.row += 1;
                lines.column = 0;
                current_line_chars = 0;
                lines_utf16_column = 0;
            } else {
                lines.column += ch.len_utf8() as u32;
                current_line_chars += 1;
                lines_utf16_column += ch.len_utf16() as u32;
            }
        }

        if !first_line_done {
            first_line_chars = current_line_chars;
        }
        let last_line_chars = current_line_chars;
        if current_line_chars > longest_row_chars {
            longest_row = lines.row;
            longest_row_chars = current_line_chars;
        }

        Self {
            len: text.len(),
            len_utf16,
            lines,
            lines_utf16: PointUtf16::new(lines.row, lines_utf16_column),
            chars,
            first_line_chars,
            last_line_chars,
            longest_row,
            longest_row_chars,
        }
    }
}

impl ContextLessSummary for TextSummary {
    fn add_summary(&mut self, other: &Self) {
        let joined_chars = self.last_line_chars + other.first_line_chars;

        let mut new_longest_row = self.longest_row;
        let mut new_longest_chars = self.longest_row_chars;

        if joined_chars > new_longest_chars {
            new_longest_row = self.lines.row;
            new_longest_chars = joined_chars;
        }

        if other.longest_row > 0 && other.longest_row_chars > new_longest_chars {
            new_longest_row = self.lines.row + other.longest_row;
            new_longest_chars = other.longest_row_chars;
        }

        if self.lines.row == 0 {
            self.first_line_chars = joined_chars;
        }

        if other.lines.row == 0 {
            self.last_line_chars = joined_chars;
        } else {
            self.last_line_chars = other.last_line_chars;
        }

        self.longest_row = new_longest_row;
        self.longest_row_chars = new_longest_chars;
        self.len += other.len;
        self.len_utf16 += other.len_utf16;
        self.lines += other.lines;
        self.lines_utf16 += other.lines_utf16;
        self.chars += other.chars;
    }
}

#[derive(Clone, Debug)]
struct Chunk {
    chars: Bitmap,
    /// One set bit per UTF-16 code unit the text encodes to.
    ///
    /// Bit `i` is set where byte `i` starts a character, and additionally at
    /// `i+1` where that character needs a surrogate pair. So the popcount is
    /// the UTF-16 length, and the bits below an offset are the code units
    /// before it, which is what every conversion into this chunk is asking
    /// for.
    chars_utf16: Bitmap,
    newlines: Bitmap,
    text: ArrayString<MAX_BASE>,
}

impl Chunk {
    fn new(text: &str) -> Self {
        let (chars, chars_utf16, newlines) = chunk_bitmaps(text);
        let mut arr = ArrayString::new();
        arr.push_str(text);

        Self {
            chars,
            chars_utf16,
            newlines,
            text: arr,
        }
    }

    fn push_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let offset = self.text.len();
        self.text.push_str(s);

        // The appended text's maps are built from bit zero and shifted into
        // place. A surrogate pair's second bit sits at `i+1` of a four-byte
        // sequence, so it never reaches past the text it was built from and the
        // shift cannot push a bit out of the chunk.
        let (chars, chars_utf16, newlines) = chunk_bitmaps(s);
        self.chars |= chars << offset;
        self.chars_utf16 |= chars_utf16 << offset;
        self.newlines |= newlines << offset;
    }

    fn len_utf16(&self) -> usize {
        self.chars_utf16.count_ones() as usize
    }

    /// Byte offset ending the line `start` falls on, exclusive of its newline.
    fn line_end_from(&self, start: usize) -> usize {
        let rest = bits_in(self.newlines, start..self.text.len());
        if rest == 0 {
            self.text.len()
        } else {
            start + rest.trailing_zeros() as usize
        }
    }

    /// Byte range of `row` within this chunk, exclusive of its newline.
    ///
    /// A row past the last one this chunk holds collapses onto the end, which
    /// is what lets a caller ask for a row whose text continues in the next
    /// chunk and get an empty range rather than a wrong one.
    fn offset_range_for_row(&self, row: u32) -> Range<usize> {
        let start = if row == 0 {
            0
        } else {
            nth_newline_offset_bitmap(self.newlines, row)
        };
        if start >= self.text.len() {
            return self.text.len()..self.text.len();
        }
        start..self.line_end_from(start)
    }

    /// Clamp `point` to this chunk's row, then to a character boundary.
    ///
    /// The boundary is a character one, not a grapheme cluster. That is the
    /// contract [`Rope::clip_offset`] has always had, and callers wanting
    /// clusters go through [`Rope::next_grapheme_boundary`] instead.
    fn clip_point(&self, point: Point, bias: Bias) -> Point {
        let row = self.offset_range_for_row(point.row);
        let len = (row.end - row.start) as u32;
        if point.column >= len {
            return Point::new(point.row, len);
        }

        let text = self.text.as_str();
        let mut column = point.column as usize;
        match bias {
            Bias::Left => {
                while column > 0 && !text.is_char_boundary(row.start + column) {
                    column -= 1;
                }
            },
            Bias::Right => {
                while column < len as usize && !text.is_char_boundary(row.start + column) {
                    column += 1;
                }
            },
        }
        Point::new(point.row, column as u32)
    }

    /// Clamp a UTF-16 column to this chunk's row, then to a character boundary.
    ///
    /// The column is converted into bytes, clipped there, and converted back,
    /// which is what makes the answer a column the rope can actually address.
    fn clip_point_utf16(&self, point: PointUtf16, bias: Bias) -> PointUtf16 {
        let row = self.offset_range_for_row(point.row);
        let bytes = self.advance_utf16(row.start, point.column as usize, row.end);
        let clipped = self.clip_point(Point::new(point.row, (bytes - row.start) as u32), bias);

        let end = row.start + clipped.column as usize;
        PointUtf16::new(
            point.row,
            bits_in(self.chars_utf16, row.start..end).count_ones(),
        )
    }

    /// Byte offset reached by advancing `units` UTF-16 code units from `start`,
    /// stopping at `limit`.
    ///
    /// A target falling between the two units of a surrogate pair rounds up to
    /// the end of that character, since the answer has to be a character
    /// boundary and the walk this replaced consumed whole characters. That is
    /// why the round-up reads the character map rather than the code-unit one.
    fn advance_utf16(&self, start: usize, units: usize, limit: usize) -> usize {
        let available = bits_in(self.chars_utf16, start..limit);
        if units == 0 || start >= limit {
            return start;
        }
        if units >= available.count_ones() as usize {
            return limit;
        }

        let unit_ix = start + nth_set_bit(available, units);
        let after = unit_ix + 1;
        let boundary = if after >= self.text.len() {
            self.text.len()
        } else {
            let run = (self.chars >> after).trailing_zeros() as usize;
            after + run.min(self.text.len() - after)
        };
        boundary.min(limit)
    }

    /// Bytes from `start` to where advancing `units` UTF-16 code units lands,
    /// stopping at the end of the line `start` falls on.
    fn line_column_bytes(&self, start: usize, units: u32) -> u32 {
        let line_end = self.line_end_from(start);
        (self.advance_utf16(start, units as usize, line_end) - start.min(line_end)) as u32
    }

    fn summarize_from_bitmaps(&self) -> TextSummary {
        let text_len = self.text.len();
        let chars = self.chars.count_ones() as usize;
        let newline_count = self.newlines.count_ones();

        let (row, column) = if newline_count == 0 {
            (0, text_len as u32)
        } else {
            let last_nl_bit = Bitmap::BITS - 1 - self.newlines.leading_zeros();
            let last_nl_byte = last_nl_bit;
            (newline_count, (text_len - 1 - last_nl_byte as usize) as u32)
        };

        let first_line_chars = if newline_count == 0 {
            chars as u32
        } else {
            let first_nl_bit = self.newlines.trailing_zeros();
            let mask = (1 as Bitmap)
                .checked_shl(first_nl_bit)
                .unwrap_or(0)
                .wrapping_sub(1);
            (self.chars & mask).count_ones()
        };

        let last_line_chars = if newline_count == 0 {
            chars as u32
        } else {
            let last_nl_bit = Bitmap::BITS - 1 - self.newlines.leading_zeros();
            let mask = !((1 as Bitmap)
                .checked_shl(last_nl_bit + 1)
                .unwrap_or(0)
                .wrapping_sub(1));
            (self.chars & mask).count_ones()
        };

        let (longest_row, longest_row_chars) =
            self.compute_longest_row(newline_count, first_line_chars, last_line_chars, row);

        let lines_utf16 = if newline_count == 0 {
            PointUtf16::new(0, self.len_utf16() as u32)
        } else {
            let last_nl_byte = (Bitmap::BITS - 1 - self.newlines.leading_zeros()) as usize;
            let utf16_col = bits_in(self.chars_utf16, last_nl_byte + 1..text_len).count_ones();
            PointUtf16::new(row, utf16_col)
        };

        TextSummary {
            len: text_len,
            len_utf16: OffsetUtf16(self.len_utf16()),
            lines: Point::new(row, column),
            lines_utf16,
            chars,
            first_line_chars,
            last_line_chars,
            longest_row,
            longest_row_chars,
        }
    }

    fn compute_longest_row(
        &self,
        newline_count: u32,
        first_line_chars: u32,
        last_line_chars: u32,
        total_rows: u32,
    ) -> (u32, u32) {
        if newline_count == 0 {
            return (0, self.chars.count_ones());
        }

        let mut best_row = 0u32;
        let mut best_chars = first_line_chars;

        if last_line_chars > best_chars {
            best_row = total_rows;
            best_chars = last_line_chars;
        }

        if newline_count >= 2 {
            let mut remaining = self.newlines;
            let mut prev_nl_bit = remaining.trailing_zeros();
            remaining &= remaining - 1;
            let mut current_row = 1u32;

            while remaining != 0 {
                let nl_bit = remaining.trailing_zeros();
                let mask_between = ((1 as Bitmap)
                    .checked_shl(nl_bit)
                    .unwrap_or(0)
                    .wrapping_sub(1))
                    & !((1 as Bitmap)
                        .checked_shl(prev_nl_bit + 1)
                        .unwrap_or(0)
                        .wrapping_sub(1));
                let line_chars = (self.chars & mask_between).count_ones();
                if line_chars > best_chars {
                    best_row = current_row;
                    best_chars = line_chars;
                }
                prev_nl_bit = nl_bit;
                remaining &= remaining - 1;
                current_row += 1;
            }
        }

        (best_row, best_chars)
    }
}

impl Item for Chunk {
    type Summary = TextSummary;

    fn summary(&self, _cx: ()) -> TextSummary {
        self.summarize_from_bitmaps()
    }
}

#[derive(Clone)]
pub struct Rope {
    chunks: SumTree<Chunk>,
}

impl Default for Rope {
    fn default() -> Self {
        Self::new()
    }
}

impl Rope {
    pub fn new() -> Self {
        Self {
            chunks: SumTree::new(()),
        }
    }

    pub fn push(&mut self, mut text: &str) {
        let mut consumed = 0usize;
        self.chunks.update_last(
            |last_chunk| {
                let available = MAX_BASE.saturating_sub(last_chunk.text.len());
                if available > 0 && !text.is_empty() {
                    let mut take = cmp::min(available, text.len());
                    while take > 0 && !text.is_char_boundary(take) {
                        take -= 1;
                    }
                    if take > 0 {
                        last_chunk.push_str(&text[..take]);
                        consumed = take;
                    }
                }
            },
            (),
        );
        text = &text[consumed..];

        while !text.is_empty() {
            let mut split_ix = cmp::min(MAX_BASE, text.len());
            while !text.is_char_boundary(split_ix) {
                split_ix -= 1;
            }
            let (chunk, remainder) = text.split_at(split_ix);
            self.chunks.push(Chunk::new(chunk), ());
            text = remainder;
        }
    }

    /// Concatenate `other` onto this rope, merging the seam when it would leave
    /// two partial chunks against each other.
    ///
    /// Every edit rebuilds a rope by appending a prefix, the new text, and a
    /// suffix, and the suffix starts wherever the cursor stopped, so it is
    /// partial by construction. Left alone those chunks never merge again, and
    /// the count drifts with the number of edits rather than the size of the
    /// text, deepening the tree for every later descent.
    pub fn append(&mut self, other: Rope) {
        let Some(incoming) = other.chunks.first().map(|chunk| chunk.text.len()) else {
            return;
        };
        // Under half a chunk on either side is the point past which merging is
        // worth it and cannot overflow, since two such chunks fit in one.
        let merge = incoming < MIN_BASE
            || self
                .chunks
                .last()
                .is_some_and(|last| last.text.len() < MIN_BASE);
        if !merge {
            self.chunks.append(other.chunks, ());
            return;
        }

        // Everything past the boundary chunk, which appends untouched.
        let rest = {
            let mut chunks = other.chunks.cursor::<()>(());
            chunks.next();
            chunks.next();
            chunks.suffix()
        };
        // The boundary chunk goes back through `push`, which is the one path
        // that fills this rope's tail before opening a new chunk.
        let first = other.chunks.first().expect("a first chunk was just read");
        self.push(first.text.as_str());
        self.chunks.append(rest, ());
    }

    pub fn cursor(&self, offset: usize) -> Cursor<'_> {
        Cursor::new(self, offset)
    }

    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        let mut new_rope = Rope::new();
        let mut cursor = self.cursor(0);
        new_rope.append(cursor.slice(range.start));
        cursor.seek_forward(range.end);
        new_rope.push(text);
        new_rope.append(cursor.suffix());
        *self = new_rope;
    }

    pub fn len(&self) -> usize {
        self.chunks.extent::<usize>(())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn summary(&self) -> &TextSummary {
        self.chunks.summary()
    }

    pub fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        let mut cursor = self.cursor(range.start);
        cursor.summary(range.end)
    }

    pub fn max_point(&self) -> Point {
        self.chunks.summary().lines
    }

    pub fn point_to_offset(&self, target: Point) -> usize {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<Point, usize>, _>((), &target, Bias::Right);
        let Dimensions(chunk_start_point, chunk_start_offset, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.len(),
        };

        let remaining_rows = target.row - chunk_start_point.row;
        if remaining_rows == 0 {
            return chunk_start_offset + (target.column - chunk_start_point.column) as usize;
        }

        let pos = nth_newline_offset_bitmap(chunk.newlines, remaining_rows);
        chunk_start_offset + pos + target.column as usize
    }

    pub fn offset_to_point(&self, offset: usize) -> Point {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<usize, Point>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start_offset, chunk_start_point, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.chunks.summary().lines,
        };

        let remaining = offset - chunk_start_offset;
        let (row_delta, col) = offset_to_point_in_chunk(chunk.newlines, remaining);
        if row_delta == 0 {
            chunk_start_point + Point::new(0, col)
        } else {
            Point::new(chunk_start_point.row + row_delta, col)
        }
    }

    pub fn offsets_to_points_batch(&self, offsets: &[usize]) -> Vec<Point> {
        let mut indexed: Vec<(usize, usize)> = offsets.iter().copied().enumerate().collect();
        indexed.sort_unstable_by_key(|&(_, off)| off);

        let mut results = vec![Point::zero(); offsets.len()];
        let mut cursor = self.chunks.cursor::<Dimensions<usize, Point>>(());
        let summary_lines = self.chunks.summary().lines;

        for (original_idx, offset) in indexed {
            cursor.seek_forward(&offset, Bias::Right);
            let Dimensions(chunk_start_offset, chunk_start_point, ()) = *cursor.start();
            results[original_idx] = match cursor.item() {
                Some(chunk) => {
                    let remaining = offset - chunk_start_offset;
                    let (row_delta, col) = offset_to_point_in_chunk(chunk.newlines, remaining);
                    if row_delta == 0 {
                        chunk_start_point + Point::new(0, col)
                    } else {
                        Point::new(chunk_start_point.row + row_delta, col)
                    }
                },
                None => summary_lines,
            };
        }
        results
    }

    pub fn points_to_offsets_batch(&self, points: &[Point]) -> Vec<usize> {
        let mut indexed: Vec<(usize, Point)> = points.iter().copied().enumerate().collect();
        indexed.sort_unstable_by_key(|a| a.1);

        let mut results = vec![0usize; points.len()];
        let mut cursor = self.chunks.cursor::<Dimensions<Point, usize>>(());
        let len = self.len();

        for (original_idx, point) in indexed {
            cursor.seek_forward(&point, Bias::Right);
            let Dimensions(chunk_start_point, chunk_start_offset, ()) = *cursor.start();
            results[original_idx] = match cursor.item() {
                Some(chunk) => {
                    let remaining_rows = point.row - chunk_start_point.row;
                    if remaining_rows == 0 {
                        chunk_start_offset + (point.column - chunk_start_point.column) as usize
                    } else {
                        let pos = nth_newline_offset_bitmap(chunk.newlines, remaining_rows);
                        chunk_start_offset + pos + point.column as usize
                    }
                },
                None => len,
            };
        }
        results
    }

    /// Byte range of `row`, exclusive of its newline.
    ///
    /// Seeking the row's end rather than its start is what keeps this to one
    /// descent. The chunk it lands on either starts on this row, meaning the
    /// row began earlier and its start is that chunk's offset less the columns
    /// already counted, or starts on an earlier row, meaning the row begins
    /// inside it.
    fn row_byte_range(&self, row: u32) -> Range<usize> {
        let max = self.max_point();
        if row > max.row {
            let len = self.len();
            return len..len;
        }

        let (start, _end, chunk_opt) = self.chunks.find::<Dimensions<Point, usize>, _>(
            (),
            &Point::new(row, u32::MAX),
            Bias::Right,
        );
        let Dimensions(chunk_start_point, chunk_start_offset, ()) = start;
        let Some(chunk) = chunk_opt else {
            let len = self.len();
            return (len - max.column as usize)..len;
        };

        let local = chunk.offset_range_for_row(row - chunk_start_point.row);
        let row_start = if chunk_start_point.row == row {
            chunk_start_offset - chunk_start_point.column as usize
        } else {
            chunk_start_offset + local.start
        };
        row_start..(chunk_start_offset + local.end)
    }

    pub fn line_len(&self, row: u32) -> u32 {
        // A row past the end has no length of its own, where clipping would
        // answer with the last row's.
        if row > self.max_point().row {
            return 0;
        }
        self.clip_point(Point::new(row, u32::MAX), Bias::Left)
            .column
    }

    /// Walk `rows` from a single cursor.
    ///
    /// The walk stops after the rope's last row, so it can yield fewer items
    /// than `rows` asks for.
    pub fn line_walk(&self, rows: Range<u32>) -> LineWalk<'_> {
        LineWalk {
            chunks: self.chunks.cursor::<usize>(()),
            offset: self.point_to_offset(Point::new(rows.start, 0)),
            row: rows.start,
            end_row: rows.end,
            len: self.len(),
            started: false,
        }
    }

    pub fn line_lens_in_range(&self, rows: Range<u32>) -> Vec<u32> {
        if rows.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(rows.len());
        let mut walk = self.line_walk(rows.clone());
        while let Some((_, len)) = walk.next_len() {
            results.push(len);
        }
        // Rows past the last one have no length of their own.
        results.resize(rows.len(), 0);
        results
    }

    pub fn chunks_in_line(&self, row: u32) -> ChunksInRange<'_> {
        let range = self.row_byte_range(row);
        self.chunks_in_range(range)
    }

    /// Clamp `point` to a position the rope actually holds.
    ///
    /// A column past the end of its row lands on the row's end, and one inside
    /// a multi-byte character moves to a character boundary in the direction of
    /// `bias`. A row past the last one collapses onto the rope's end.
    ///
    /// Seeking by [`Point`] carries the row's columns across chunk boundaries,
    /// so a row spanning chunks still resolves in the one descent this takes.
    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        let (start, _end, chunk_opt) = self.chunks.find::<Point, _>((), &point, Bias::Right);
        match chunk_opt {
            Some(chunk) => start + chunk.clip_point(point - start, bias),
            None => self.chunks.summary().lines,
        }
    }

    pub fn lines(&self) -> Lines<'_> {
        Lines {
            rope: self,
            current_row: 0,
            max_row: self.max_point().row,
        }
    }

    pub fn line_at_row(&self, row: u32) -> String {
        let range = self.row_byte_range(row);
        if range.is_empty() {
            return String::new();
        }
        let mut result = String::with_capacity(range.end - range.start);
        for chunk in self.chunks_in_range(range) {
            result.push_str(chunk);
        }
        result
    }

    pub fn chars_at(&self, offset: usize) -> CharsAt<'_> {
        let mut chunks = self.chunks.cursor::<usize>(());
        chunks.seek(&offset, Bias::Right);
        let local_offset = match chunks.item() {
            Some(_) => offset - *chunks.start(),
            None => 0,
        };
        CharsAt {
            chunks,
            local_offset,
        }
    }

    pub fn reversed_chars_at(&self, offset: usize) -> ReversedCharsAt<'_> {
        let mut chunks = self.chunks.cursor::<usize>(());
        chunks.seek(&offset, Bias::Right);
        let local_offset = match chunks.item() {
            Some(_) => offset - *chunks.start(),
            None => {
                chunks.prev();
                match chunks.item() {
                    Some(chunk) => chunk.text.len(),
                    None => 0,
                }
            },
        };
        ReversedCharsAt {
            chunks,
            local_offset,
        }
    }

    pub fn chars(&self) -> CharsAt<'_> {
        self.chars_at(0)
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        if offset == 0 || offset == self.len() {
            return true;
        }
        if offset > self.len() {
            return false;
        }
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<usize, Point>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start_offset, _, ()) = start;
        let chunk = match chunk_opt {
            Some(c) => c,
            None => return true,
        };
        let local = offset - chunk_start_offset;
        chunk.text.as_str().is_char_boundary(local)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        let offset = offset.min(self.len());
        if self.is_char_boundary(offset) {
            return offset;
        }
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<usize, Point>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start_offset, _, ()) = start;
        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.len(),
        };
        let local = offset - chunk_start_offset;
        let text = chunk.text.as_str();
        let clipped_local = match bias {
            Bias::Left => {
                let mut c = local;
                while c > 0 && !text.is_char_boundary(c) {
                    c -= 1;
                }
                c
            },
            Bias::Right => {
                let mut c = local;
                while c < text.len() && !text.is_char_boundary(c) {
                    c += 1;
                }
                c
            },
        };
        chunk_start_offset + clipped_local
    }

    /// Offset of the first grapheme-cluster boundary after `offset`, or
    /// `offset` itself at the rope end.
    ///
    /// A cluster is what a reader calls one character. A base plus its
    /// combining marks, an emoji ZWJ sequence, a regional-indicator flag pair,
    /// and a skin-tone modifier each form exactly one. Stepping by scalar
    /// instead lands the cursor inside one of those and lets a delete take it
    /// apart. An `offset` off a char boundary is clipped left before stepping.
    ///
    /// See also:
    /// - [`Self::prev_grapheme_boundary`] for the backward step.
    pub fn next_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset, Bias::Left);
        if offset >= self.len() {
            return offset;
        }

        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        let mut pos = offset;
        loop {
            let Some((chunk, chunk_start)) = self.chunk_at(pos) else {
                return offset;
            };
            match cursor.next_boundary(chunk, chunk_start) {
                Ok(Some(boundary)) => return boundary,
                // `NextChunk` only fires when the chunk ends before the rope
                // does, so the follow-up chunk is always there.
                Err(GraphemeIncomplete::NextChunk) => pos = chunk_start + chunk.len(),
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let Some((ctx, ctx_start)) = self.chunk_ending_at(end) else {
                        return offset;
                    };
                    cursor.provide_context(ctx, ctx_start);
                },
                Ok(None) | Err(_) => return offset,
            }
        }
    }

    /// Offset of the first grapheme-cluster boundary before `offset`, or
    /// `offset` itself at the rope start.
    ///
    /// Backward mirror of [`Self::next_grapheme_boundary`], with the same
    /// cluster definition and the same left-clipping of an `offset` that is not
    /// on a char boundary.
    pub fn prev_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset, Bias::Left);
        if offset == 0 {
            return 0;
        }

        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        let mut pos = offset - 1;
        loop {
            let Some((chunk, chunk_start)) = self.chunk_at(pos) else {
                return offset;
            };
            match cursor.prev_boundary(chunk, chunk_start) {
                Ok(Some(boundary)) => return boundary,
                Err(GraphemeIncomplete::PrevChunk) => {
                    if chunk_start == 0 {
                        return offset;
                    }
                    pos = chunk_start - 1;
                },
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let Some((ctx, ctx_start)) = self.chunk_ending_at(end) else {
                        return offset;
                    };
                    cursor.provide_context(ctx, ctx_start);
                },
                Ok(None) | Err(_) => return offset,
            }
        }
    }

    /// The chunk holding `offset` paired with the offset it starts at, or
    /// `None` past the last chunk.
    fn chunk_at(&self, offset: usize) -> Option<(&str, usize)> {
        let (start, _end, chunk) =
            self.chunks
                .find::<Dimensions<usize, Point>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start, _, ()) = start;
        chunk.map(|chunk| (chunk.text.as_str(), chunk_start))
    }

    /// The chunk truncated so it ends exactly at `end`, paired with the offset
    /// it starts at.
    ///
    /// `GraphemeCursor::provide_context` asserts the chunk handed back ends at
    /// the offset its `PreContext` named, so the chunk holding `end - 1` has to
    /// be cut down rather than passed whole.
    fn chunk_ending_at(&self, end: usize) -> Option<(&str, usize)> {
        let (chunk, chunk_start) = self.chunk_at(end.checked_sub(1)?)?;
        let local_end = end.checked_sub(chunk_start)?;
        chunk.get(..local_end).map(|ctx| (ctx, chunk_start))
    }

    pub fn starts_with(&self, s: &str) -> bool {
        if s.len() > self.len() {
            return false;
        }
        let mut remaining = s.as_bytes();
        for chunk in self.chunks() {
            if remaining.is_empty() {
                return true;
            }
            let take = remaining.len().min(chunk.len());
            if chunk.as_bytes()[..take] != remaining[..take] {
                return false;
            }
            remaining = &remaining[take..];
        }
        remaining.is_empty()
    }

    pub fn ends_with(&self, s: &str) -> bool {
        if s.len() > self.len() {
            return false;
        }
        let mut remaining = s.chars().rev();
        let mut rope_chars = self.reversed_chars_at(self.len());
        for expected in &mut remaining {
            match rope_chars.next() {
                Some(actual) if actual == expected => {},
                _ => return false,
            }
        }
        true
    }

    pub fn find(&self, needle: &str, start: usize) -> Option<usize> {
        if needle.is_empty() {
            return Some(start.min(self.len()));
        }
        if start >= self.len() {
            return None;
        }
        let needle_bytes = needle.as_bytes();
        let nlen = needle_bytes.len();
        let mut buf: Vec<u8> = Vec::with_capacity(nlen + MAX_BASE);
        let mut buf_start = start;

        for chunk in self.chunks_in_range(start..self.len()) {
            buf.extend_from_slice(chunk.as_bytes());
            if let Some(pos) = buf.windows(nlen).position(|w| w == needle_bytes) {
                return Some(buf_start + pos);
            }
            if buf.len() >= nlen {
                let keep = nlen - 1;
                let drain = buf.len() - keep;
                buf_start += drain;
                buf.copy_within(drain.., 0);
                buf.truncate(keep);
            }
        }
        None
    }

    pub fn find_iter<'a>(&'a self, needle: &'a str) -> FindIter<'a> {
        FindIter {
            rope: self,
            needle,
            pos: 0,
        }
    }

    pub fn find_all(&self, needle: &str) -> Vec<usize> {
        self.find_iter(needle).collect()
    }

    pub fn count_occurrences(&self, needle: &str) -> usize {
        self.find_iter(needle).count()
    }

    pub fn replace_all(&mut self, needle: &str, replacement: &str) {
        if needle.is_empty() {
            return;
        }
        let positions = self.find_all(needle);
        if positions.is_empty() {
            return;
        }
        let nlen = needle.len();
        let mut new_rope = Rope::new();
        let mut last_end = 0;
        for &pos in &positions {
            if pos > last_end {
                new_rope.append(self.slice(last_end..pos));
            }
            new_rope.push(replacement);
            last_end = pos + nlen;
        }
        if last_end < self.len() {
            new_rope.append(self.slice(last_end..self.len()));
        }
        *self = new_rope;
    }

    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        ChunksIter {
            cursor: self.chunks.cursor::<usize>(()),
            started: false,
        }
    }

    pub fn chunks_in_range(&self, range: Range<usize>) -> ChunksInRange<'_> {
        ChunksInRange {
            chunks: self.chunks.cursor::<usize>(()),
            range,
            started: false,
        }
    }

    /// This rope's chunks as a haystack the regex automata can walk in place.
    ///
    /// Offsets it reports are rope offsets. Most callers want
    /// [`Self::regex_input`], which wraps this and carries the search range.
    pub fn regex_cursor(&self) -> RegexChunks<'_> {
        let mut chunks = self.chunks.cursor::<usize>(());
        chunks.next();
        RegexChunks {
            chunks,
            len: self.len(),
        }
    }

    /// An [`Input`] over this rope, searching only `range`.
    ///
    /// The range rides on the input rather than on the haystack, so a match
    /// comes back in rope offsets rather than offsets into a slice.
    pub fn regex_input(&self, range: Range<usize>) -> Input<RegexChunks<'_>> {
        Input::new(self.regex_cursor()).range(range)
    }

    pub fn reversed_chunks_in_range(&self, range: Range<usize>) -> ReversedChunksInRange<'_> {
        let mut chunks = self.chunks.cursor::<usize>(());
        chunks.seek(&range.end, Bias::Right);
        if chunks.item().is_none() || *chunks.start() >= range.end {
            chunks.prev();
        }
        ReversedChunksInRange { chunks, range }
    }

    pub fn slice_rows(&self, range: Range<u32>) -> Rope {
        let start = self.point_to_offset(Point::new(range.start, 0));
        let end = if range.end > self.max_point().row {
            self.len()
        } else {
            self.point_to_offset(Point::new(range.end, 0))
        };
        let mut cursor = self.cursor(start);
        cursor.slice(end)
    }

    pub fn point_to_point_utf16(&self, target: Point) -> PointUtf16 {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<Point, PointUtf16>, _>((), &target, Bias::Right);
        let Dimensions(chunk_start_point, chunk_start_utf16, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.chunks.summary().lines_utf16,
        };

        let text = chunk.text.as_str();
        let remaining_rows = target.row - chunk_start_point.row;
        let line_start = if remaining_rows == 0 {
            0
        } else {
            nth_newline_offset_bitmap(chunk.newlines, remaining_rows)
        };

        let col_bytes = if remaining_rows == 0 {
            (target.column - chunk_start_point.column) as usize
        } else {
            target.column as usize
        };

        let scan_end = (line_start + col_bytes).min(text.len());
        let utf16_col = bits_in(chunk.chars_utf16, line_start..scan_end).count_ones();

        chunk_start_utf16 + PointUtf16::new(remaining_rows, utf16_col)
    }

    pub fn point_utf16_to_point(&self, target: PointUtf16) -> Point {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<PointUtf16, Point>, _>((), &target, Bias::Right);
        let Dimensions(chunk_start_utf16, chunk_start_point, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.chunks.summary().lines,
        };

        let remaining_rows = target.row - chunk_start_utf16.row;
        let line_start = if remaining_rows == 0 {
            0
        } else {
            nth_newline_offset_bitmap(chunk.newlines, remaining_rows)
        };

        let remaining_utf16_col = if remaining_rows == 0 {
            target.column - chunk_start_utf16.column
        } else {
            target.column
        };

        let byte_col = chunk.line_column_bytes(line_start, remaining_utf16_col);

        chunk_start_point + Point::new(remaining_rows, byte_col)
    }

    pub fn offset_to_offset_utf16(&self, offset: usize) -> OffsetUtf16 {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<usize, OffsetUtf16>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start_offset, chunk_start_utf16, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.chunks.summary().len_utf16,
        };

        let remaining = offset - chunk_start_offset;
        let utf16_delta = (chunk.chars_utf16 & below(remaining)).count_ones() as usize;

        OffsetUtf16(chunk_start_utf16.0 + utf16_delta)
    }

    pub fn offset_utf16_to_offset(&self, target: OffsetUtf16) -> usize {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<OffsetUtf16, usize>, _>((), &target, Bias::Right);
        let Dimensions(chunk_start_utf16, chunk_start_offset, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.len(),
        };

        let remaining_utf16 = target.0 - chunk_start_utf16.0;
        let byte_offset = chunk.advance_utf16(0, remaining_utf16, chunk.text.len());

        chunk_start_offset + byte_offset
    }

    /// Clamp `point` to a UTF-16 position the rope actually holds.
    ///
    /// The byte-space counterpart of [`Self::clip_point`], and the same single
    /// descent, where composing the two conversions around it cost three.
    pub fn clip_point_utf16(&self, point: PointUtf16, bias: Bias) -> PointUtf16 {
        let (start, _end, chunk_opt) = self.chunks.find::<PointUtf16, _>((), &point, Bias::Right);
        match chunk_opt {
            Some(chunk) => start + chunk.clip_point_utf16(point - start, bias),
            None => self.chunks.summary().lines_utf16,
        }
    }

    pub fn slice(&self, range: Range<usize>) -> Rope {
        let mut cursor = self.cursor(range.start);
        cursor.slice(range.end)
    }

    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<usize, PointUtf16>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start_offset, chunk_start_utf16, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.chunks.summary().lines_utf16,
        };

        let remaining = offset - chunk_start_offset;
        let seen = below(remaining);
        let row_delta = (chunk.newlines & seen).count_ones();
        let line_start = if row_delta == 0 {
            0
        } else {
            (Bitmap::BITS - (chunk.newlines & seen).leading_zeros()) as usize
        };
        let utf16_col = bits_in(chunk.chars_utf16, line_start..remaining).count_ones();

        chunk_start_utf16 + PointUtf16::new(row_delta, utf16_col)
    }

    pub fn point_utf16_to_offset(&self, target: PointUtf16) -> usize {
        let (start, _end, chunk_opt) =
            self.chunks
                .find::<Dimensions<PointUtf16, usize>, _>((), &target, Bias::Right);
        let Dimensions(chunk_start_utf16, chunk_start_offset, ()) = start;

        let chunk = match chunk_opt {
            Some(c) => c,
            None => return self.len(),
        };

        let remaining_rows = target.row - chunk_start_utf16.row;
        let line_start = if remaining_rows == 0 {
            0
        } else {
            nth_newline_offset_bitmap(chunk.newlines, remaining_rows)
        };

        let remaining_utf16_col = if remaining_rows == 0 {
            target.column - chunk_start_utf16.column
        } else {
            target.column
        };

        let byte_offset = chunk.line_column_bytes(line_start, remaining_utf16_col) as usize;

        chunk_start_offset + line_start + byte_offset
    }

    pub fn max_point_utf16(&self) -> PointUtf16 {
        self.chunks.summary().lines_utf16
    }

    pub fn bytes_in_range(&self, range: Range<usize>) -> BytesInRange<'_> {
        BytesInRange {
            chunks: self.chunks_in_range(range),
            current: &[],
            pos: 0,
        }
    }
}

impl std::fmt::Display for Rope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

struct ChunksIter<'a> {
    cursor: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    started: bool,
}

impl<'a> Iterator for ChunksIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            self.cursor.seek(&0usize, Bias::Right);
        } else {
            self.cursor.next();
        }
        self.cursor.item().map(|chunk| chunk.text.as_str())
    }
}

/// Walks consecutive rows from a single cursor.
///
/// Reading rows one at a time costs a tree descent each. This positions one
/// cursor and carries it forward, finding each line break as a set bit in a
/// chunk's newline map, so a screenful of rows costs one descent rather than
/// one per row.
///
/// Rows come out in order and the walk ends after the rope's last row, so a
/// caller asking for more rows than exist gets fewer items rather than empty
/// ones.
pub struct LineWalk<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    /// Byte offset the next row starts at.
    offset: usize,
    /// Index of the next row.
    row: u32,
    /// One past the last row to report.
    end_row: u32,
    len: usize,
    started: bool,
}

impl<'a> LineWalk<'a> {
    /// Append the next row's text to `out`, returning its byte length.
    ///
    /// The row's newline is not appended. Appending rather than returning a
    /// string is what lets a caller walking many rows keep one allocation.
    pub fn next_into(&mut self, out: &mut String) -> Option<u32> {
        self.step(Some(out))
    }

    /// The next row's index and byte length, without reading its text.
    pub fn next_len(&mut self) -> Option<(u32, u32)> {
        let row = self.row;
        self.step(None).map(|len| (row, len))
    }

    /// Advance past the next row, reporting its byte length and appending its
    /// text to `out` when one is given.
    ///
    /// The text is taken as the scan passes over it, so wanting a row's text
    /// costs nothing beyond the copy itself.
    fn step(&mut self, mut out: Option<&mut String>) -> Option<u32> {
        if self.row >= self.end_row || self.offset > self.len {
            return None;
        }
        if !self.started {
            self.started = true;
            self.chunks.seek(&self.offset, Bias::Right);
        }

        let start = self.offset;
        let mut pos = start;
        loop {
            let Some(chunk) = self.chunks.item() else {
                // Past the last chunk, so the row runs to the end of the rope
                // and there is no row after it.
                self.offset = self.len + 1;
                self.row += 1;
                return Some((self.len - start) as u32);
            };

            let chunk_start = *self.chunks.start();
            let text = chunk.text.as_str();
            let chunk_end = chunk_start + text.len();
            if pos >= chunk_end {
                self.chunks.next();
                continue;
            }

            let local = pos - chunk_start;
            let rest = chunk.newlines >> local;
            if rest != 0 {
                let end = local + rest.trailing_zeros() as usize;
                if let Some(out) = out.as_mut() {
                    out.push_str(&text[local..end]);
                }
                self.offset = chunk_start + end + 1;
                self.row += 1;
                return Some((chunk_start + end - start) as u32);
            }

            if let Some(out) = out.as_mut() {
                out.push_str(&text[local..]);
            }
            pos = chunk_end;
            self.chunks.next();
        }
    }
}

pub struct ChunksInRange<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    range: Range<usize>,
    started: bool,
}

impl<'a> Iterator for ChunksInRange<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            self.chunks.seek(&self.range.start, Bias::Right);
        } else {
            self.chunks.next();
        }

        let chunk = self.chunks.item()?;
        let chunk_start = *self.chunks.start();
        if chunk_start >= self.range.end {
            return None;
        }

        let local_start = self.range.start.saturating_sub(chunk_start);
        let chunk_end = chunk_start + chunk.text.len();
        let local_end = self.range.end.min(chunk_end) - chunk_start;

        Some(&chunk.text.as_str()[local_start..local_end])
    }
}

/// A [`Rope`]'s chunks as a haystack the regex automata can walk without the
/// rope being flattened first.
///
/// Spans the whole rope rather than a slice of it, so an offset it reports is a
/// rope offset and a match needs no translating back. Restrict a search with
/// the range on the [`Input`] instead, which is what [`Rope::regex_input`]
/// does.
///
/// See also:
/// - [`Rope::regex_cursor`] to build one.
pub struct RegexChunks<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    len: usize,
}

impl RegexCursor for RegexChunks<'_> {
    fn chunk(&self) -> &[u8] {
        self.chunks
            .item()
            .map(|chunk| chunk.text.as_bytes())
            .unwrap_or_default()
    }

    /// Chunks never split a codepoint, so every regex feature is available.
    fn utf8_aware(&self) -> bool {
        true
    }

    fn advance(&mut self) -> bool {
        // Peeked rather than stepped and undone, the trait requiring a failed
        // step to leave the chunk exactly where it was.
        if self.chunks.next_item().is_none() {
            return false;
        }
        self.chunks.next();
        true
    }

    fn backtrack(&mut self) -> bool {
        if self.chunks.prev_item().is_none() {
            return false;
        }
        self.chunks.prev();
        true
    }

    fn total_bytes(&self) -> Option<usize> {
        Some(self.len)
    }

    fn offset(&self) -> usize {
        *self.chunks.start()
    }
}

pub struct ReversedChunksInRange<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    range: Range<usize>,
}

impl<'a> Iterator for ReversedChunksInRange<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.start >= self.range.end {
            return None;
        }

        let chunk = self.chunks.item()?;
        let chunk_start = *self.chunks.start();
        let chunk_end = chunk_start + chunk.text.len();

        if chunk_end <= self.range.start {
            return None;
        }

        let local_start = self.range.start.saturating_sub(chunk_start);
        let local_end = self.range.end.min(chunk_end) - chunk_start;
        let result = &chunk.text.as_str()[local_start..local_end];

        self.chunks.prev();
        Some(result)
    }
}

pub struct CharsAt<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    local_offset: usize,
}

impl Iterator for CharsAt<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        loop {
            let chunk = self.chunks.item()?;
            let text = &chunk.text.as_str()[self.local_offset..];
            if let Some(ch) = text.chars().next() {
                self.local_offset += ch.len_utf8();
                return Some(ch);
            }
            self.chunks.next();
            self.local_offset = 0;
        }
    }
}

pub struct ReversedCharsAt<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    local_offset: usize,
}

impl Iterator for ReversedCharsAt<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        loop {
            let chunk = self.chunks.item()?;
            let text = &chunk.text.as_str()[..self.local_offset];
            if let Some(ch) = text.chars().next_back() {
                self.local_offset -= ch.len_utf8();
                return Some(ch);
            }
            self.chunks.prev();
            match self.chunks.item() {
                Some(chunk) => self.local_offset = chunk.text.len(),
                None => return None,
            }
        }
    }
}

pub struct BytesInRange<'a> {
    chunks: ChunksInRange<'a>,
    current: &'a [u8],
    pos: usize,
}

impl Iterator for BytesInRange<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        loop {
            if self.pos < self.current.len() {
                let byte = self.current[self.pos];
                self.pos += 1;
                return Some(byte);
            }
            let chunk = self.chunks.next()?;
            self.current = chunk.as_bytes();
            self.pos = 0;
        }
    }
}

pub struct FindIter<'a> {
    rope: &'a Rope,
    needle: &'a str,
    pos: usize,
}

impl Iterator for FindIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.needle.is_empty() {
            return None;
        }
        let result = self.rope.find(self.needle, self.pos)?;
        self.pos = result + self.needle.len();
        Some(result)
    }
}

pub struct Lines<'a> {
    rope: &'a Rope,
    current_row: u32,
    max_row: u32,
}

impl<'a> Iterator for Lines<'a> {
    type Item = ChunksInLine<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_row > self.max_row {
            return None;
        }
        let row = self.current_row;
        self.current_row += 1;
        Some(ChunksInLine {
            inner: self.rope.chunks_in_line(row),
        })
    }
}

pub struct ChunksInLine<'a> {
    inner: ChunksInRange<'a>,
}

impl<'a> Iterator for ChunksInLine<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        self.inner.next()
    }
}

impl From<&str> for Rope {
    fn from(text: &str) -> Self {
        let mut rope = Rope::new();
        rope.push(text);
        rope
    }
}

pub struct Cursor<'a> {
    rope: &'a Rope,
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    offset: usize,
}

impl<'a> Cursor<'a> {
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn new(rope: &'a Rope, offset: usize) -> Self {
        let mut chunks = rope.chunks.cursor::<usize>(());
        chunks.seek(&offset, Bias::Right);
        Self {
            rope,
            chunks,
            offset,
        }
    }

    pub fn seek_forward(&mut self, offset: usize) {
        self.chunks.seek_forward(&offset, Bias::Right);
        self.offset = offset;
    }

    pub fn summary(&mut self, end_offset: usize) -> TextSummary {
        let mut result = TextSummary::default();

        let chunk = match self.chunks.item() {
            Some(c) => c,
            None => {
                self.offset = end_offset;
                return result;
            },
        };

        let chunk_start = *self.chunks.start();
        let local_start = self.offset - chunk_start;
        let chunk_end = chunk_start + chunk.text.len();

        if end_offset <= chunk_end {
            let local_end = end_offset - chunk_start;
            if local_start < local_end {
                result = TextSummary::from_str(&chunk.text[local_start..local_end]);
            }
            self.offset = end_offset;
            return result;
        }

        if local_start < chunk.text.len() {
            let partial = TextSummary::from_str(&chunk.text[local_start..]);
            ContextLessSummary::add_summary(&mut result, &partial);
        }
        self.chunks.next();

        let middle: TextSummary = self.chunks.summary(&end_offset, Bias::Right);
        ContextLessSummary::add_summary(&mut result, &middle);

        if let Some(chunk) = self.chunks.item() {
            let chunk_start = *self.chunks.start();
            if end_offset > chunk_start {
                let local_end = end_offset - chunk_start;
                let partial = TextSummary::from_str(&chunk.text[..local_end]);
                ContextLessSummary::add_summary(&mut result, &partial);
            }
        }

        self.offset = end_offset;
        result
    }

    pub fn slice(&mut self, end_offset: usize) -> Rope {
        let mut slice = Rope::new();

        if let Some(chunk) = self.chunks.item() {
            let start_ix = self.offset - *self.chunks.start();
            let end_ix = end_offset.min(self.chunks.end()) - *self.chunks.start();
            if start_ix < end_ix {
                slice.push(&chunk.text[start_ix..end_ix]);
            }
        }

        if end_offset > self.chunks.end() {
            self.chunks.next();
            slice
                .chunks
                .append(self.chunks.slice(&end_offset, Bias::Right), ());

            if let Some(chunk) = self.chunks.item() {
                let end_ix = end_offset - *self.chunks.start();
                if end_ix > 0 {
                    slice.push(&chunk.text[..end_ix]);
                }
            }
        }

        self.offset = end_offset;
        slice
    }

    pub fn suffix(mut self) -> Rope {
        self.slice(self.rope.len())
    }
}

impl<'a> Dimension<'a, TextSummary> for usize {
    fn zero(_cx: ()) -> Self {
        0
    }

    fn add_summary(&mut self, summary: &'a TextSummary, _cx: ()) {
        *self += summary.len;
    }
}

/// Mask of every bit below `offset`, saturating at the full width.
fn below(offset: usize) -> Bitmap {
    if offset >= MAX_BASE {
        !0
    } else {
        ((1 as Bitmap) << offset) - 1
    }
}

/// The bits of `map` covering `range`, shifted down to bit zero.
fn bits_in(map: Bitmap, range: Range<usize>) -> Bitmap {
    if range.start >= MAX_BASE {
        return 0;
    }
    (map & below(range.end)) >> range.start
}

/// Index of the `n`th set bit, counting from one.
///
/// Splitting at 64 keeps the kernel below on a word the parallel bit count is
/// written for. [`Bitmap`] is narrowed under test so chunk boundaries stay
/// reachable, so it is widened here rather than at each call.
fn nth_set_bit(v: Bitmap, n: usize) -> usize {
    #[cfg(test)]
    let v = v as u128;
    #[cfg(not(test))]
    let v: u128 = v;

    let low = v as u64;
    let low_count = low.count_ones() as usize;
    if n > low_count {
        64 + nth_set_bit_u64((v >> 64) as u64, (n - low_count) as u64) as usize
    } else {
        nth_set_bit_u64(low, n as u64) as usize
    }
}

/// Index of the `n`th set bit of `v`, counting from one.
///
/// A binary search over the parallel-bit-count intermediates, narrowing the
/// answer one power of two at a time. The subtract-and-mask in each step is
/// what keeps it branchless, and so constant-time regardless of `n`.
fn nth_set_bit_u64(v: u64, mut n: u64) -> u64 {
    let v = v.reverse_bits();
    let mut s: u64 = 64;

    let a = v - ((v >> 1) & (u64::MAX / 3));
    let b = (a & (u64::MAX / 5)) + ((a >> 2) & (u64::MAX / 5));
    let c = (b + (b >> 4)) & (u64::MAX / 0x11);
    let d = (c + (c >> 8)) & (u64::MAX / 0x101);

    let t = (d >> 32) + (d >> 48);
    s -= (t.wrapping_sub(n) & 256) >> 3;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (d >> (s - 16)) & 0xff;
    s -= (t.wrapping_sub(n) & 256) >> 4;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (c >> (s - 8)) & 0xf;
    s -= (t.wrapping_sub(n) & 256) >> 5;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (b >> (s - 4)) & 0x7;
    s -= (t.wrapping_sub(n) & 256) >> 6;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (a >> (s - 2)) & 0x3;
    s -= (t.wrapping_sub(n) & 256) >> 7;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (v >> (s - 1)) & 0x1;
    s -= (t.wrapping_sub(n) & 256) >> 8;

    65 - s - 1
}

/// The character, UTF-16 code unit, and newline maps for `text`, positioned
/// from bit zero.
///
/// Bytes are taken a lane at a time so the per-byte work is an 8-bit shift
/// rather than one over the full mask width, and the lanes are assembled at the
/// end.
fn chunk_bitmaps(text: &str) -> (Bitmap, Bitmap, Bitmap) {
    const LANE: usize = 8;
    let mut char_lanes = [0u8; MAX_BASE / LANE];
    let mut wide_lanes = [0u8; MAX_BASE / LANE];
    let mut newline_lanes = [0u8; MAX_BASE / LANE];

    for (lane_ix, lane) in text.as_bytes().chunks(LANE).enumerate() {
        let (mut chars, mut wide, mut newlines) = (0u8, 0u8, 0u8);
        for (ix, &byte) in lane.iter().enumerate() {
            chars |= u8::from(byte & 0xC0 != 0x80) << ix;
            newlines |= u8::from(byte == b'\n') << ix;
            // A byte this large opens a four-byte sequence, which is the only
            // encoding costing two UTF-16 code units.
            wide |= u8::from(byte >= 240) << ix;
        }
        char_lanes[lane_ix] = chars;
        wide_lanes[lane_ix] = wide;
        newline_lanes[lane_ix] = newlines;
    }

    let chars = Bitmap::from_le_bytes(char_lanes);
    (
        chars,
        (Bitmap::from_le_bytes(wide_lanes) << 1) | chars,
        Bitmap::from_le_bytes(newline_lanes),
    )
}

/// Byte offset just past the `n`th newline, counting from one.
fn nth_newline_offset_bitmap(newlines: Bitmap, n: u32) -> usize {
    nth_set_bit(newlines, n as usize) + 1
}

fn offset_to_point_in_chunk(newlines: Bitmap, remaining: usize) -> (u32, u32) {
    if remaining == 0 {
        return (0, 0);
    }
    let mask: Bitmap = if remaining as u32 >= Bitmap::BITS {
        !0
    } else {
        ((1 as Bitmap) << remaining) - 1
    };
    let nl = newlines & mask;
    let row_delta = nl.count_ones();
    if row_delta == 0 {
        (0, remaining as u32)
    } else {
        let last_nl_pos = Bitmap::BITS - 1 - nl.leading_zeros();
        (row_delta, (remaining - 1 - last_nl_pos as usize) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string_empty() {
        let rope = Rope::new();
        assert_eq!(rope.to_string(), "");
    }

    #[test]
    fn to_string_single_push() {
        let mut rope = Rope::new();
        rope.push("hello world");
        assert_eq!(rope.to_string(), "hello world");
    }

    #[test]
    fn to_string_multiple_pushes() {
        let mut rope = Rope::new();
        rope.push("hello ");
        rope.push("world");
        assert_eq!(rope.to_string(), "hello world");
    }

    #[test]
    fn to_string_after_append() {
        let mut rope1 = Rope::new();
        rope1.push("hello ");

        let mut rope2 = Rope::new();
        rope2.push("world");

        rope1.append(rope2);
        assert_eq!(rope1.to_string(), "hello world");
    }

    #[test]
    fn to_string_unicode() {
        let mut rope = Rope::new();
        rope.push("h\u{00e9}llo \u{4e16}\u{754c}");
        assert_eq!(rope.to_string(), "h\u{00e9}llo \u{4e16}\u{754c}");
    }

    #[test]
    fn chunks_iteration() {
        let mut rope = Rope::new();
        rope.push("chunk1");
        rope.push("chunk2");
        rope.push("chunk3");

        let chunks: Vec<&str> = rope.chunks().collect();
        assert_eq!(chunks.join(""), "chunk1chunk2chunk3");
    }

    #[test]
    fn replace_mid_chunk() {
        let mut rope = Rope::from("hello world");
        rope.replace(0..5, "goodbye");
        assert_eq!(rope.to_string(), "goodbye world");
    }

    #[test]
    fn replace_spanning_chunks() {
        let mut rope = Rope::new();
        rope.push("hello ");
        rope.push("world");
        rope.replace(3..8, "XYZ");
        assert_eq!(rope.to_string(), "helXYZrld");
    }

    #[test]
    fn replace_at_end() {
        let mut rope = Rope::from("hello");
        rope.replace(5..5, " world");
        assert_eq!(rope.to_string(), "hello world");
    }

    #[test]
    fn replace_entire_content() {
        let mut rope = Rope::from("hello world");
        rope.replace(0..11, "goodbye");
        assert_eq!(rope.to_string(), "goodbye");
    }

    #[test]
    fn replace_delete_only() {
        let mut rope = Rope::from("hello world");
        rope.replace(5..6, "");
        assert_eq!(rope.to_string(), "helloworld");
    }

    #[test]
    fn push_splits_large_text() {
        let mut rope = Rope::new();
        let large_text = "a".repeat(MAX_BASE * 3);
        rope.push(&large_text);

        let chunks: Vec<_> = rope.chunks().collect();
        assert!(
            chunks.len() >= 3,
            "large text should be split into multiple chunks"
        );
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.len() <= MAX_BASE);
        }
    }

    #[test]
    fn push_respects_char_boundaries() {
        let mut rope = Rope::new();
        // \u{4e16} is 3 bytes (Chinese character for "world")
        let text = "a".repeat(MAX_BASE - 2) + "\u{4e16}" + &"b".repeat(MAX_BASE);
        rope.push(&text);
        assert_eq!(rope.to_string(), text);
    }

    #[test]
    fn push_fills_last_chunk() {
        let mut rope = Rope::new();
        rope.push("hello");
        rope.push(" world");

        let chunks: Vec<_> = rope.chunks().collect();
        assert_eq!(chunks.len(), 1, "small pushes should fill same chunk");
        assert_eq!(rope.to_string(), "hello world");
    }

    #[test]
    fn text_summary_single_line() {
        let s = TextSummary::from_str("hello");
        assert_eq!(s.len, 5);
        assert_eq!(s.chars, 5);
        assert_eq!(s.lines, Point::new(0, 5));
        assert_eq!(s.first_line_chars, 5);
        assert_eq!(s.last_line_chars, 5);
        assert_eq!(s.longest_row, 0);
        assert_eq!(s.longest_row_chars, 5);
    }

    #[test]
    fn text_summary_multiline() {
        let s = TextSummary::from_str("ab\ncdef\ng");
        assert_eq!(s.len, 9);
        assert_eq!(s.chars, 9);
        assert_eq!(s.lines, Point::new(2, 1));
        assert_eq!(s.first_line_chars, 2);
        assert_eq!(s.last_line_chars, 1);
        assert_eq!(s.longest_row, 1);
        assert_eq!(s.longest_row_chars, 4);
    }

    #[test]
    fn text_summary_empty() {
        let s = TextSummary::from_str("");
        assert_eq!(s.len, 0);
        assert_eq!(s.chars, 0);
        assert_eq!(s.lines, Point::zero());
        assert_eq!(s.first_line_chars, 0);
        assert_eq!(s.last_line_chars, 0);
        assert_eq!(s.longest_row_chars, 0);
    }

    #[test]
    fn text_summary_trailing_newline() {
        let s = TextSummary::from_str("abc\n");
        assert_eq!(s.lines, Point::new(1, 0));
        assert_eq!(s.first_line_chars, 3);
        assert_eq!(s.last_line_chars, 0);
        assert_eq!(s.longest_row, 0);
        assert_eq!(s.longest_row_chars, 3);
    }

    #[test]
    fn text_summary_multibyte() {
        let s = TextSummary::from_str("h\u{00e9}llo");
        assert_eq!(s.len, 6);
        assert_eq!(s.chars, 5);
        assert_eq!(s.lines, Point::new(0, 6));
        assert_eq!(s.first_line_chars, 5);
        assert_eq!(s.last_line_chars, 5);
    }

    fn combine(a: &str, b: &str) -> TextSummary {
        let mut s = TextSummary::from_str(a);
        ContextLessSummary::add_summary(&mut s, &TextSummary::from_str(b));
        s
    }

    #[test]
    fn add_summary_line_joining() {
        let s = combine("abc", "def");
        assert_eq!(s.first_line_chars, 6);
        assert_eq!(s.last_line_chars, 6);
        assert_eq!(s.longest_row_chars, 6);
        assert_eq!(s.chars, 6);
        assert_eq!(s.lines, Point::new(0, 6));
    }

    #[test]
    fn add_summary_with_newline() {
        let s = combine("abc\n", "de");
        assert_eq!(s.first_line_chars, 3);
        assert_eq!(s.last_line_chars, 2);
        assert_eq!(s.longest_row, 0);
        assert_eq!(s.longest_row_chars, 3);
        assert_eq!(s.lines, Point::new(1, 2));
    }

    #[test]
    fn add_summary_joined_becomes_longest() {
        let s = combine("ab\ncde", "fgh\ni");
        // Joined line: "cde" + "fgh" = 6 chars
        assert_eq!(s.first_line_chars, 2);
        assert_eq!(s.last_line_chars, 1);
        assert_eq!(s.longest_row, 1);
        assert_eq!(s.longest_row_chars, 6);
    }

    #[test]
    fn bitmap_ascii() {
        let chunk = Chunk::new("hello");
        assert_eq!(chunk.chars.count_ones(), 5);
        assert_eq!(chunk.newlines, 0);
    }

    #[test]
    fn bitmap_multibyte() {
        let chunk = Chunk::new("h\u{00e9}"); // é is 2 bytes
        assert_eq!(chunk.chars.count_ones(), 2);
        assert_eq!(chunk.text.len(), 3);
    }

    #[test]
    fn bitmap_newlines() {
        let chunk = Chunk::new("a\tb\nc");
        assert_eq!(chunk.chars.count_ones(), 5);
        assert_eq!(chunk.newlines.count_ones(), 1);
    }

    #[test]
    fn bitmap_summarize_matches_from_str() {
        let cases = [
            "",
            "hello",
            "ab\ncdef\ng",
            "abc\n",
            "\n",
            "\n\n\n",
            "h\u{00e9}llo",
            "\t\t\n  x\ny",
            "a\nb\nc\nd\ne",
            "\u{4e16}\u{754c}",
            "a\u{1F600}b",
        ];
        for text in cases {
            if text.len() > MAX_BASE {
                continue;
            }
            let chunk = Chunk::new(text);
            let bitmap_summary = chunk.summarize_from_bitmaps();
            let str_summary = TextSummary::from_str(text);
            assert_eq!(
                bitmap_summary.len, str_summary.len,
                "len mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.len_utf16, str_summary.len_utf16,
                "len_utf16 mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.lines, str_summary.lines,
                "lines mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.chars, str_summary.chars,
                "chars mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.first_line_chars, str_summary.first_line_chars,
                "first_line_chars mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.last_line_chars, str_summary.last_line_chars,
                "last_line_chars mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.longest_row, str_summary.longest_row,
                "longest_row mismatch for {text:?}"
            );
            assert_eq!(
                bitmap_summary.longest_row_chars, str_summary.longest_row_chars,
                "longest_row_chars mismatch for {text:?}"
            );
        }
    }

    #[test]
    fn point_to_offset_single_line() {
        let rope = Rope::from("hello");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(0, 3)), 3);
        assert_eq!(rope.point_to_offset(Point::new(0, 5)), 5);
    }

    #[test]
    fn point_to_offset_multiline() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(0, 2)), 2);
        assert_eq!(rope.point_to_offset(Point::new(1, 0)), 4);
        assert_eq!(rope.point_to_offset(Point::new(1, 2)), 6);
        assert_eq!(rope.point_to_offset(Point::new(2, 0)), 8);
        assert_eq!(rope.point_to_offset(Point::new(2, 3)), 11);
    }

    #[test]
    fn point_to_offset_unicode() {
        // "hé" = [0x68, 0xC3, 0xA9] = 3 bytes
        let rope = Rope::from("hé\nworld");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(0, 3)), 3);
        assert_eq!(rope.point_to_offset(Point::new(1, 0)), 4);
        assert_eq!(rope.point_to_offset(Point::new(1, 5)), 9);
    }

    #[test]
    fn point_to_offset_past_end() {
        let rope = Rope::from("hello");
        assert_eq!(rope.point_to_offset(Point::new(1, 0)), 5);
    }

    #[test]
    fn offset_to_point_single_line() {
        let rope = Rope::from("hello");
        assert_eq!(rope.offset_to_point(0), Point::new(0, 0));
        assert_eq!(rope.offset_to_point(3), Point::new(0, 3));
        assert_eq!(rope.offset_to_point(5), Point::new(0, 5));
    }

    #[test]
    fn offset_to_point_multiline() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.offset_to_point(0), Point::new(0, 0));
        assert_eq!(rope.offset_to_point(2), Point::new(0, 2));
        assert_eq!(rope.offset_to_point(4), Point::new(1, 0));
        assert_eq!(rope.offset_to_point(6), Point::new(1, 2));
        assert_eq!(rope.offset_to_point(8), Point::new(2, 0));
        assert_eq!(rope.offset_to_point(11), Point::new(2, 3));
    }

    #[test]
    fn offset_to_point_unicode() {
        let rope = Rope::from("hé\nworld");
        assert_eq!(rope.offset_to_point(0), Point::new(0, 0));
        assert_eq!(rope.offset_to_point(3), Point::new(0, 3));
        assert_eq!(rope.offset_to_point(4), Point::new(1, 0));
    }

    #[test]
    fn roundtrip_point_offset() {
        let rope = Rope::from("abc\ndef\nghi");
        for offset in 0..=rope.len() {
            let point = rope.offset_to_point(offset);
            assert_eq!(
                rope.point_to_offset(point),
                offset,
                "roundtrip failed for offset {offset}"
            );
        }
    }

    #[test]
    fn max_point_empty() {
        let rope = Rope::new();
        assert_eq!(rope.max_point(), Point::zero());
    }

    #[test]
    fn max_point_single_line() {
        let rope = Rope::from("hello");
        assert_eq!(rope.max_point(), Point::new(0, 5));
    }

    #[test]
    fn max_point_trailing_newline() {
        let rope = Rope::from("abc\n");
        assert_eq!(rope.max_point(), Point::new(1, 0));
    }

    #[test]
    fn max_point_multiline() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.max_point(), Point::new(2, 3));
    }

    #[test]
    fn line_len_various() {
        let rope = Rope::from("abc\nde\nfghij");
        assert_eq!(rope.line_len(0), 3);
        assert_eq!(rope.line_len(1), 2);
        assert_eq!(rope.line_len(2), 5);
        assert_eq!(rope.line_len(3), 0);
    }

    #[test]
    fn line_len_empty() {
        let rope = Rope::from("a\n\nb");
        assert_eq!(rope.line_len(0), 1);
        assert_eq!(rope.line_len(1), 0);
        assert_eq!(rope.line_len(2), 1);
    }

    #[test]
    fn chunks_in_line_basic() {
        let rope = Rope::from("hello\nworld\nfoo");
        let line: String = rope.chunks_in_line(1).collect();
        assert_eq!(line, "world");
    }

    #[test]
    fn clip_point_past_end() {
        let rope = Rope::from("hello\nhi");
        assert_eq!(
            rope.clip_point(Point::new(5, 0), Bias::Left),
            Point::new(1, 2)
        );
        assert_eq!(
            rope.clip_point(Point::new(0, 100), Bias::Left),
            Point::new(0, 5)
        );
    }

    #[test]
    fn clip_point_multibyte() {
        let rope = Rope::from("h\u{00e9}llo");
        assert_eq!(
            rope.clip_point(Point::new(0, 2), Bias::Left),
            Point::new(0, 1)
        );
        assert_eq!(
            rope.clip_point(Point::new(0, 2), Bias::Right),
            Point::new(0, 3)
        );
    }

    #[test]
    fn clip_point_mid_char_boundary() {
        // "hé" = [0x68, 0xC3, 0xA9]
        let rope = Rope::from("hé");
        // col 2 is in the middle of 'é' (byte 0xA9)
        assert_eq!(
            rope.clip_point(Point::new(0, 2), Bias::Left),
            Point::new(0, 1)
        );
        assert_eq!(
            rope.clip_point(Point::new(0, 2), Bias::Right),
            Point::new(0, 3)
        );
    }

    #[test]
    fn line_at_row_first() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.line_at_row(0), "abc");
    }

    #[test]
    fn line_at_row_middle() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.line_at_row(1), "def");
    }

    #[test]
    fn line_at_row_last() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.line_at_row(2), "ghi");
    }

    #[test]
    fn line_at_row_past_end() {
        let rope = Rope::from("abc");
        assert_eq!(rope.line_at_row(5), "");
    }

    #[test]
    fn line_at_row_trailing_newline() {
        let rope = Rope::from("abc\n");
        assert_eq!(rope.line_at_row(0), "abc");
        assert_eq!(rope.line_at_row(1), "");
    }

    #[test]
    fn row_byte_range_consistency() {
        let mut rope = Rope::new();
        rope.push("line0\nline1\nline2\nline3");
        let text = rope.to_string();
        for row in 0..=rope.max_point().row {
            let range = rope.row_byte_range(row);
            let line = &text[range];
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn chars_at_start() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.chars_at(0).collect();
        assert_eq!(chars, vec!['h', 'e', 'l', 'l', 'o']);
    }

    #[test]
    fn chars_at_mid() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.chars_at(2).collect();
        assert_eq!(chars, vec!['l', 'l', 'o']);
    }

    #[test]
    fn chars_at_end() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.chars_at(5).collect();
        assert_eq!(chars, Vec::<char>::new());
    }

    #[test]
    fn chars_at_unicode() {
        // "hé世" = h(1) + é(2) + 世(3) = 6 bytes
        let rope = Rope::from("hé世");
        let chars: Vec<char> = rope.chars_at(1).collect();
        assert_eq!(chars, vec!['é', '世']);
    }

    #[test]
    fn reversed_chars_at_end() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.reversed_chars_at(5).collect();
        assert_eq!(chars, vec!['o', 'l', 'l', 'e', 'h']);
    }

    #[test]
    fn reversed_chars_at_mid() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.reversed_chars_at(3).collect();
        assert_eq!(chars, vec!['l', 'e', 'h']);
    }

    #[test]
    fn reversed_chars_at_start() {
        let rope = Rope::from("hello");
        let chars: Vec<char> = rope.reversed_chars_at(0).collect();
        assert_eq!(chars, Vec::<char>::new());
    }

    #[test]
    fn reversed_chars_at_unicode() {
        let rope = Rope::from("hé世");
        // offset 3 = after 'é' (h=1byte, é=2bytes)
        let chars: Vec<char> = rope.reversed_chars_at(3).collect();
        assert_eq!(chars, vec!['é', 'h']);
    }

    #[test]
    fn cursor_offset() {
        let rope = Rope::from("hello");
        let cursor = rope.cursor(3);
        assert_eq!(cursor.offset(), 3);
    }

    #[test]
    fn chars_from_zero() {
        let rope = Rope::from("abc");
        let chars: Vec<char> = rope.chars().collect();
        assert_eq!(chars, vec!['a', 'b', 'c']);
    }

    #[test]
    fn is_char_boundary_valid() {
        let rope = Rope::from("h\u{00e9}\u{4e16}");
        assert!(rope.is_char_boundary(0));
        assert!(rope.is_char_boundary(1));
        assert!(!rope.is_char_boundary(2));
        assert!(rope.is_char_boundary(3));
        assert!(!rope.is_char_boundary(4));
        assert!(!rope.is_char_boundary(5));
        assert!(rope.is_char_boundary(6));
        assert!(!rope.is_char_boundary(7));
    }

    #[test]
    fn clip_offset_on_boundary() {
        let rope = Rope::from("h\u{00e9}\u{4e16}");
        assert_eq!(rope.clip_offset(0, Bias::Left), 0);
        assert_eq!(rope.clip_offset(1, Bias::Left), 1);
        assert_eq!(rope.clip_offset(3, Bias::Left), 3);
        assert_eq!(rope.clip_offset(6, Bias::Left), 6);
    }

    #[test]
    fn clip_offset_mid_char() {
        let rope = Rope::from("h\u{00e9}\u{4e16}");
        assert_eq!(rope.clip_offset(2, Bias::Left), 1);
        assert_eq!(rope.clip_offset(2, Bias::Right), 3);
        assert_eq!(rope.clip_offset(4, Bias::Left), 3);
        assert_eq!(rope.clip_offset(4, Bias::Right), 6);
    }

    #[test]
    fn clip_offset_clamps() {
        let rope = Rope::from("abc");
        assert_eq!(rope.clip_offset(100, Bias::Left), 3);
    }

    #[test]
    fn starts_with_match() {
        let rope = Rope::from("hello world");
        assert!(rope.starts_with("hello"));
        assert!(rope.starts_with(""));
        assert!(rope.starts_with("hello world"));
    }

    #[test]
    fn starts_with_mismatch() {
        let rope = Rope::from("hello world");
        assert!(!rope.starts_with("world"));
        assert!(!rope.starts_with("hello world!"));
    }

    #[test]
    fn ends_with_match() {
        let rope = Rope::from("hello world");
        assert!(rope.ends_with("world"));
        assert!(rope.ends_with(""));
        assert!(rope.ends_with("hello world"));
    }

    #[test]
    fn ends_with_mismatch() {
        let rope = Rope::from("hello world");
        assert!(!rope.ends_with("hello"));
        assert!(!rope.ends_with("!hello world"));
    }

    #[test]
    fn chunks_in_range_full() {
        let rope = Rope::from("hello world");
        let text: String = rope.chunks_in_range(0..rope.len()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn chunks_in_range_subrange() {
        let rope = Rope::from("hello world");
        let text: String = rope.chunks_in_range(3..8).collect();
        assert_eq!(text, "lo wo");
    }

    #[test]
    fn chunks_in_range_empty() {
        let rope = Rope::from("hello");
        let text: String = rope.chunks_in_range(3..3).collect();
        assert_eq!(text, "");
    }

    #[test]
    fn reversed_chunks_in_range_full() {
        let rope = Rope::from("hello");
        let chunks: Vec<&str> = rope.reversed_chunks_in_range(0..rope.len()).collect();
        assert_eq!(chunks.concat(), "hello");
    }

    #[test]
    fn reversed_chunks_in_range_subrange() {
        let rope = Rope::from("hello world");
        let chunks: Vec<&str> = rope.reversed_chunks_in_range(3..8).collect();
        let text: String = chunks.into_iter().rev().collect();
        assert_eq!(text, "lo wo");
    }

    #[test]
    fn reversed_chunks_in_range_empty() {
        let rope = Rope::from("hello");
        let chunks: Vec<&str> = rope.reversed_chunks_in_range(3..3).collect();
        assert!(chunks.is_empty());
    }

    #[test]
    fn slice_rows_single() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.slice_rows(1..2).to_string(), "def\n");
    }

    #[test]
    fn slice_rows_multi() {
        let rope = Rope::from("abc\ndef\nghi");
        assert_eq!(rope.slice_rows(0..2).to_string(), "abc\ndef\n");
    }

    #[test]
    fn slice_rows_past_end() {
        let rope = Rope::from("abc\ndef");
        assert_eq!(rope.slice_rows(1..100).to_string(), "def");
    }

    #[test]
    fn point_utf16_add_same_row() {
        let a = PointUtf16::new(1, 5);
        let b = PointUtf16::new(0, 3);
        assert_eq!(a + b, PointUtf16::new(1, 8));
    }

    #[test]
    fn point_utf16_add_cross_row() {
        let a = PointUtf16::new(1, 5);
        let b = PointUtf16::new(2, 3);
        assert_eq!(a + b, PointUtf16::new(3, 3));
    }

    #[test]
    fn point_utf16_ord() {
        assert!(PointUtf16::new(0, 5) < PointUtf16::new(1, 0));
        assert!(PointUtf16::new(1, 3) < PointUtf16::new(1, 5));
        assert!(PointUtf16::new(2, 0) > PointUtf16::new(1, 100));
    }

    #[test]
    fn point_utf16_roundtrip_ascii() {
        let rope = Rope::from("abc\ndef");
        for row in 0..=1 {
            let len = rope.line_len(row);
            for col in 0..=len {
                let point = Point::new(row, col);
                let utf16 = rope.point_to_point_utf16(point);
                let back = rope.point_utf16_to_point(utf16);
                assert_eq!(back, point, "roundtrip failed for {point:?}");
            }
        }
    }

    #[test]
    fn point_utf16_roundtrip_bmp() {
        let rope = Rope::from("h\u{00e9}\nw\u{00f6}rld");
        let p = Point::new(0, 3);
        let utf16 = rope.point_to_point_utf16(p);
        assert_eq!(utf16, PointUtf16::new(0, 2));
        assert_eq!(rope.point_utf16_to_point(utf16), p);
    }

    #[test]
    fn point_utf16_roundtrip_surrogate() {
        // \u{10000} is 4 bytes UTF-8, 2 code units UTF-16
        let rope = Rope::from("a\u{10000}b");
        let p = Point::new(0, 5);
        let utf16 = rope.point_to_point_utf16(p);
        assert_eq!(utf16, PointUtf16::new(0, 3));
        let back = rope.point_utf16_to_point(utf16);
        assert_eq!(back, p);
    }

    #[test]
    fn offset_utf16_roundtrip() {
        let rope = Rope::from("a\u{10000}b\nc\u{00e9}");
        for offset in 0..=rope.len() {
            if !rope.is_char_boundary(offset) {
                continue;
            }
            let utf16 = rope.offset_to_offset_utf16(offset);
            let back = rope.offset_utf16_to_offset(utf16);
            assert_eq!(back, offset, "roundtrip failed for offset {offset}");
        }
    }

    #[test]
    fn text_summary_lines_utf16_ascii() {
        let s = TextSummary::from_str("abc\ndef");
        assert_eq!(s.lines_utf16, PointUtf16::new(1, 3));
    }

    #[test]
    fn text_summary_lines_utf16_bmp() {
        let s = TextSummary::from_str("h\u{00e9}\nw\u{00f6}rld");
        assert_eq!(s.lines_utf16, PointUtf16::new(1, 5));
    }

    #[test]
    fn text_summary_lines_utf16_surrogate() {
        // a(1) + \u{10000}(2) + b(1) = 4 UTF-16 code units
        let s = TextSummary::from_str("a\u{10000}b");
        assert_eq!(s.lines_utf16, PointUtf16::new(0, 4));
    }

    #[test]
    fn bitmap_summarize_lines_utf16_matches() {
        let cases = ["hello", "h\u{00e9}", "a\u{10000}b", "abc\ndef", "\n\n", ""];
        for text in cases {
            if text.len() > MAX_BASE {
                continue;
            }
            let chunk = Chunk::new(text);
            let bitmap = chunk.summarize_from_bitmaps();
            let from_str = TextSummary::from_str(text);
            assert_eq!(
                bitmap.lines_utf16, from_str.lines_utf16,
                "lines_utf16 mismatch for {text:?}"
            );
        }
    }

    #[test]
    fn clip_point_utf16_valid() {
        let rope = Rope::from("a\u{10000}b");
        let clipped = rope.clip_point_utf16(PointUtf16::new(0, 1), Bias::Left);
        assert_eq!(clipped, PointUtf16::new(0, 1));
    }

    #[test]
    fn cursor_summary_single_chunk() {
        let rope = Rope::from("hello");
        let mut cursor = rope.cursor(0);
        let summary = cursor.summary(5);
        assert_eq!(summary.len, 5);
        assert_eq!(summary.chars, 5);
        assert_eq!(summary.lines, Point::new(0, 5));
    }

    #[test]
    fn cursor_summary_cross_chunk() {
        let mut rope = Rope::new();
        let large = "a".repeat(MAX_BASE + 5);
        rope.push(&large);
        let mut cursor = rope.cursor(3);
        let summary = cursor.summary(MAX_BASE + 2);
        let expected = TextSummary::from_str(&large[3..MAX_BASE + 2]);
        assert_eq!(summary.len, expected.len);
        assert_eq!(summary.chars, expected.chars);
    }

    #[test]
    fn cursor_summary_partial() {
        let rope = Rope::from("abc\ndef");
        let mut cursor = rope.cursor(1);
        let summary = cursor.summary(5);
        assert_eq!(summary.len, 4);
        assert_eq!(summary.lines, Point::new(1, 1));
    }

    #[test]
    fn cursor_summary_empty_range() {
        let rope = Rope::from("hello");
        let mut cursor = rope.cursor(3);
        let summary = cursor.summary(3);
        assert_eq!(summary.len, 0);
    }

    #[test]
    fn slice_range() {
        let rope = Rope::from("hello world");
        let sliced = rope.slice(3..8);
        assert_eq!(sliced.to_string(), "lo wo");
    }

    #[test]
    fn slice_range_empty() {
        let rope = Rope::from("hello");
        let sliced = rope.slice(3..3);
        assert_eq!(sliced.to_string(), "");
    }

    #[test]
    fn slice_range_full() {
        let rope = Rope::from("hello");
        let sliced = rope.slice(0..5);
        assert_eq!(sliced.to_string(), "hello");
    }

    #[test]
    fn offset_to_point_utf16_ascii() {
        let rope = Rope::from("abc\ndef");
        assert_eq!(rope.offset_to_point_utf16(0), PointUtf16::new(0, 0));
        assert_eq!(rope.offset_to_point_utf16(3), PointUtf16::new(0, 3));
        assert_eq!(rope.offset_to_point_utf16(4), PointUtf16::new(1, 0));
        assert_eq!(rope.offset_to_point_utf16(7), PointUtf16::new(1, 3));
    }

    #[test]
    fn offset_to_point_utf16_bmp() {
        let rope = Rope::from("h\u{00e9}\nw");
        assert_eq!(rope.offset_to_point_utf16(0), PointUtf16::new(0, 0));
        assert_eq!(rope.offset_to_point_utf16(1), PointUtf16::new(0, 1));
        assert_eq!(rope.offset_to_point_utf16(3), PointUtf16::new(0, 2));
        assert_eq!(rope.offset_to_point_utf16(4), PointUtf16::new(1, 0));
    }

    #[test]
    fn offset_to_point_utf16_supplementary() {
        let rope = Rope::from("a\u{10000}b");
        assert_eq!(rope.offset_to_point_utf16(0), PointUtf16::new(0, 0));
        assert_eq!(rope.offset_to_point_utf16(1), PointUtf16::new(0, 1));
        assert_eq!(rope.offset_to_point_utf16(5), PointUtf16::new(0, 3));
        assert_eq!(rope.offset_to_point_utf16(6), PointUtf16::new(0, 4));
    }

    #[test]
    fn point_utf16_to_offset_ascii() {
        let rope = Rope::from("abc\ndef");
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 0)), 0);
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 3)), 3);
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(1, 0)), 4);
    }

    #[test]
    fn point_utf16_to_offset_supplementary() {
        let rope = Rope::from("a\u{10000}b");
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 1)), 1);
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 3)), 5);
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 4)), 6);
    }

    #[test]
    fn offset_point_utf16_roundtrip() {
        let rope = Rope::from("a\u{10000}b\nc\u{00e9}");
        for offset in 0..=rope.len() {
            if !rope.is_char_boundary(offset) {
                continue;
            }
            let utf16 = rope.offset_to_point_utf16(offset);
            let back = rope.point_utf16_to_offset(utf16);
            assert_eq!(back, offset, "roundtrip failed for offset {offset}");
        }
    }

    #[test]
    fn max_point_utf16_empty() {
        let rope = Rope::new();
        assert_eq!(rope.max_point_utf16(), PointUtf16::zero());
    }

    #[test]
    fn max_point_utf16_multiline() {
        let rope = Rope::from("abc\ndef");
        assert_eq!(rope.max_point_utf16(), PointUtf16::new(1, 3));
    }

    #[test]
    fn bytes_in_range_full() {
        let rope = Rope::from("hello");
        let bytes: Vec<u8> = rope.bytes_in_range(0..5).collect();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn bytes_in_range_subrange() {
        let rope = Rope::from("hello world");
        let bytes: Vec<u8> = rope.bytes_in_range(3..8).collect();
        assert_eq!(bytes, b"lo wo");
    }

    #[test]
    fn bytes_in_range_empty() {
        let rope = Rope::from("hello");
        let bytes: Vec<u8> = rope.bytes_in_range(3..3).collect();
        assert!(bytes.is_empty());
    }

    #[test]
    fn lines_iterator() {
        let rope = Rope::from("hello\nworld\nfoo");
        let lines: Vec<String> = rope.lines().map(|l| l.collect()).collect();
        assert_eq!(lines, vec!["hello", "world", "foo"]);
    }

    #[test]
    fn lines_iterator_single_line() {
        let rope = Rope::from("hello");
        let lines: Vec<String> = rope.lines().map(|l| l.collect()).collect();
        assert_eq!(lines, vec!["hello"]);
    }

    #[test]
    fn lines_iterator_empty() {
        let rope = Rope::from("");
        let lines: Vec<String> = rope.lines().map(|l| l.collect()).collect();
        assert_eq!(lines, vec![""]);
    }

    #[test]
    fn find_empty_needle() {
        let rope = Rope::from("hello");
        assert_eq!(rope.find("", 0), Some(0));
        assert_eq!(rope.find("", 3), Some(3));
        assert_eq!(rope.find("", 10), Some(5));
    }

    #[test]
    fn find_at_start() {
        let rope = Rope::from("hello world");
        assert_eq!(rope.find("hello", 0), Some(0));
    }

    #[test]
    fn find_at_end() {
        let rope = Rope::from("hello world");
        assert_eq!(rope.find("world", 0), Some(6));
    }

    #[test]
    fn find_not_found() {
        let rope = Rope::from("hello world");
        assert_eq!(rope.find("xyz", 0), None);
    }

    #[test]
    fn find_single_char() {
        let rope = Rope::from("abcdef");
        assert_eq!(rope.find("d", 0), Some(3));
        assert_eq!(rope.find("d", 4), None);
    }

    #[test]
    fn find_cross_chunk() {
        let mut rope = Rope::new();
        // MAX_BASE is 16 in test mode, so push enough to span chunks
        rope.push("abcdefghijklmnop");
        rope.push("qrstuvwxyz");
        assert_eq!(rope.find("opqr", 0), Some(14));
    }

    #[test]
    fn offsets_to_points_batch_basic() {
        let rope = Rope::from("hello\nworld\nfoo");
        let points = rope.offsets_to_points_batch(&[0, 5, 6, 11, 12, 15]);
        assert_eq!(
            points,
            vec![
                Point::new(0, 0),
                Point::new(0, 5),
                Point::new(1, 0),
                Point::new(1, 5),
                Point::new(2, 0),
                Point::new(2, 3),
            ]
        );
    }

    #[test]
    fn offsets_to_points_batch_unsorted() {
        let rope = Rope::from("ab\ncd\nef");
        let points = rope.offsets_to_points_batch(&[6, 0, 3]);
        assert_eq!(
            points,
            vec![Point::new(2, 0), Point::new(0, 0), Point::new(1, 0)]
        );
    }

    #[test]
    fn find_returns_none_past_end() {
        let rope = Rope::from("hello");
        assert_eq!(rope.find("hello", 5), None);
        assert_eq!(rope.find("hello", 100), None);
    }

    #[test]
    fn find_all_basic() {
        let rope = Rope::from("abcabc");
        assert_eq!(rope.find_all("abc"), vec![0, 3]);
    }

    #[test]
    fn find_all_non_overlapping() {
        let rope = Rope::from("aaa");
        assert_eq!(rope.find_all("aa"), vec![0]);
    }

    #[test]
    fn find_all_empty_needle() {
        let rope = Rope::from("hello");
        assert_eq!(rope.find_all(""), Vec::<usize>::new());
    }

    #[test]
    fn replace_all_basic() {
        let mut rope = Rope::from("hello world hello");
        rope.replace_all("hello", "hi");
        assert_eq!(rope.to_string(), "hi world hi");
    }

    #[test]
    fn replace_all_no_match() {
        let mut rope = Rope::from("hello");
        rope.replace_all("xyz", "abc");
        assert_eq!(rope.to_string(), "hello");
    }

    #[test]
    fn replace_all_empty_needle() {
        let mut rope = Rope::from("hello");
        rope.replace_all("", "abc");
        assert_eq!(rope.to_string(), "hello");
    }

    #[test]
    fn replace_all_different_lengths() {
        let mut rope = Rope::from("aXbXc");
        rope.replace_all("X", "YYY");
        assert_eq!(rope.to_string(), "aYYYbYYYc");

        let mut rope = Rope::from("aXXXbXXXc");
        rope.replace_all("XXX", "Y");
        assert_eq!(rope.to_string(), "aYbYc");
    }

    #[test]
    fn line_lens_in_range_matches_individual() {
        let rope = Rope::from("hello\nworld\nfoo\nbar");
        let batch = rope.line_lens_in_range(0..4);
        let individual: Vec<u32> = (0..4).map(|r| rope.line_len(r)).collect();
        assert_eq!(batch, individual);
    }

    #[test]
    fn line_lens_in_range_empty() {
        let rope = Rope::from("hello\nworld");
        assert_eq!(rope.line_lens_in_range(0..0), Vec::<u32>::new());
    }

    #[test]
    fn find_iter_basic() {
        let rope = Rope::from("abcabc");
        let results: Vec<usize> = rope.find_iter("abc").collect();
        assert_eq!(results, rope.find_all("abc"));
    }

    #[test]
    fn find_iter_lazy_stops_early() {
        let rope = Rope::from("abcabcabc");
        let mut iter = rope.find_iter("abc");
        assert_eq!(iter.next(), Some(0));
        assert_eq!(iter.next(), Some(3));
    }

    #[test]
    fn find_iter_empty_needle() {
        let rope = Rope::from("hello");
        let results: Vec<usize> = rope.find_iter("").collect();
        assert!(results.is_empty());
    }

    #[test]
    fn count_occurrences_basic() {
        let rope = Rope::from("abcabc");
        assert_eq!(rope.count_occurrences("abc"), 2);
    }

    #[test]
    fn count_occurrences_empty() {
        let rope = Rope::from("hello");
        assert_eq!(rope.count_occurrences(""), 0);
    }

    #[test]
    fn points_to_offsets_batch_basic() {
        let rope = Rope::from("hello\nworld\nfoo");
        let points = [
            Point::new(0, 0),
            Point::new(0, 5),
            Point::new(1, 0),
            Point::new(1, 5),
            Point::new(2, 0),
            Point::new(2, 3),
        ];
        let offsets = rope.points_to_offsets_batch(&points);
        let expected: Vec<usize> = points.iter().map(|&p| rope.point_to_offset(p)).collect();
        assert_eq!(offsets, expected);
    }

    #[test]
    fn points_to_offsets_batch_unsorted() {
        let rope = Rope::from("ab\ncd\nef");
        let points = [Point::new(2, 0), Point::new(0, 0), Point::new(1, 0)];
        let offsets = rope.points_to_offsets_batch(&points);
        assert_eq!(offsets, vec![6, 0, 3]);
    }

    /// Every cluster boundary in `text`, walked forward from 0 and backward
    /// from the end, so both steppers are pinned against one expectation.
    fn assert_cluster_walk(text: &str, expected: &[usize]) {
        let rope = Rope::from(text);

        let mut forward = vec![0];
        let mut offset = 0;
        while offset < rope.len() {
            let next = rope.next_grapheme_boundary(offset);
            assert!(
                next > offset,
                "forward walk stalled at {offset} in {text:?}"
            );
            forward.push(next);
            offset = next;
        }
        assert_eq!(forward, expected, "forward cluster walk over {text:?}");

        let mut backward = vec![rope.len()];
        let mut offset = rope.len();
        while offset > 0 {
            let prev = rope.prev_grapheme_boundary(offset);
            assert!(
                prev < offset,
                "backward walk stalled at {offset} in {text:?}"
            );
            backward.push(prev);
            offset = prev;
        }
        backward.reverse();
        assert_eq!(backward, expected, "backward cluster walk over {text:?}");
    }

    #[test]
    fn combining_mark_joins_its_base() {
        assert_cluster_walk("ae\u{301}b", &[0, 1, 4, 5]);
    }

    #[test]
    fn zwj_sequence_is_one_cluster() {
        assert_cluster_walk(
            "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b",
            &[0, 1, 19, 20],
        );
    }

    #[test]
    fn regional_indicators_pair_into_flags() {
        assert_cluster_walk("\u{1F1F7}\u{1F1F8}\u{1F1EE}\u{1F1F4}", &[0, 8, 16]);
    }

    #[test]
    fn skin_tone_modifier_joins_its_base() {
        assert_cluster_walk("\u{1F44D}\u{1F3FD}!", &[0, 8, 9]);
    }

    #[test]
    fn ascii_steps_one_byte_at_a_time() {
        assert_cluster_walk("hello", &[0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn stepping_stops_at_the_rope_ends() {
        let rope = Rope::from("hi");
        assert_eq!(rope.next_grapheme_boundary(2), 2, "no step past the end");
        assert_eq!(
            rope.prev_grapheme_boundary(0),
            0,
            "no step before the start"
        );

        let empty = Rope::from("");
        assert_eq!(empty.next_grapheme_boundary(0), 0);
        assert_eq!(empty.prev_grapheme_boundary(0), 0);
    }

    #[test]
    fn an_offset_inside_a_scalar_clips_left_before_stepping() {
        let rope = Rope::from("e\u{301}b");
        assert_eq!(
            rope.next_grapheme_boundary(2),
            3,
            "an offset mid-scalar clips back onto the cluster it splits",
        );
        assert_eq!(rope.prev_grapheme_boundary(2), 0);
    }

    /// A cluster split across rope chunks makes `GraphemeCursor` ask for
    /// pre-context and for neighbouring chunks, exercising the paths a
    /// single-chunk rope never reaches. Chunks cap at `MAX_BASE` bytes, which
    /// is 16 under `cfg(test)`, so the padding only has to clear that.
    #[test]
    fn clusters_spanning_chunk_boundaries_still_resolve() {
        for pad in 0..24usize {
            let text = format!(
                "{}\u{1F1F7}\u{1F1F8}e\u{301}{}",
                "a".repeat(pad),
                "b".repeat(24),
            );
            let mut rope = Rope::new();
            for ch in text.chars() {
                rope.push(&ch.to_string());
            }
            assert!(
                rope.chunks().count() > 1,
                "pad {pad} must build a multi-chunk rope"
            );

            let flag = pad;
            let decomposed = flag + 8;
            let tail = decomposed + 3;
            assert_eq!(
                rope.next_grapheme_boundary(flag),
                decomposed,
                "the flag pair is one cluster at pad {pad}",
            );
            assert_eq!(
                rope.next_grapheme_boundary(decomposed),
                tail,
                "the decomposed e keeps its combining mark at pad {pad}",
            );
            assert_eq!(
                rope.prev_grapheme_boundary(tail),
                decomposed,
                "stepping back off the tail lands on the decomposed e at pad {pad}",
            );
            assert_eq!(
                rope.prev_grapheme_boundary(decomposed),
                flag,
                "stepping back over the flag pair takes both halves at pad {pad}",
            );
        }
    }
}

/// Reference conversions written as plain char walks, and a randomized check
/// that the rope agrees with them.
///
/// The rope answers these from chunk bitmaps, which is fast but easy to get
/// subtly wrong at a chunk boundary or around a character that encodes to two
/// UTF-16 code units. These walk the text directly instead, so they are slow,
/// obviously correct, and independent of whatever representation the chunks
/// use.
#[cfg(test)]
mod utf16_reference {
    use super::*;

    /// The branchless kernel is opaque enough that a scan is worth keeping
    /// beside it.
    ///
    /// It is exercised directly rather than through [`nth_set_bit`], whose
    /// input narrows to sixteen bits under test and so would never reach the
    /// upper half of a word.
    #[test]
    fn nth_set_bit_matches_a_scan() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..200 {
            // Sparse and dense words both, since the search narrows by
            // population and a word of one set bit is a different path through
            // it than a word of sixty.
            let v = match case % 3 {
                0 => next(),
                1 => next() & next() & next(),
                _ => next() | next() | next(),
            };
            let set: Vec<u64> = (0..64).filter(|ix| v >> ix & 1 == 1).collect();
            for (rank, &ix) in set.iter().enumerate() {
                assert_eq!(
                    nth_set_bit_u64(v, rank as u64 + 1),
                    ix,
                    "bit {} of {v:#018x}",
                    rank + 1
                );
            }
        }
    }

    fn offset_to_offset_utf16(text: &str, offset: usize) -> OffsetUtf16 {
        OffsetUtf16(text[..offset].chars().map(char::len_utf16).sum())
    }

    /// Advance `target` UTF-16 units from the start of `text`.
    ///
    /// Whole characters are consumed while the running count is below the
    /// target, so a target falling between the two units of a surrogate pair
    /// lands after the character rather than inside it.
    fn offset_utf16_to_offset(text: &str, target: OffsetUtf16) -> usize {
        let mut utf16 = 0usize;
        let mut offset = 0usize;
        for ch in text.chars() {
            if utf16 >= target.0 {
                break;
            }
            utf16 += ch.len_utf16();
            offset += ch.len_utf8();
        }
        offset
    }

    fn offset_to_point_utf16(text: &str, offset: usize) -> PointUtf16 {
        let mut row = 0u32;
        let mut column = 0u32;
        for ch in text[..offset].chars() {
            if ch == '\n' {
                row += 1;
                column = 0;
            } else {
                column += ch.len_utf16() as u32;
            }
        }
        PointUtf16::new(row, column)
    }

    fn point_to_point_utf16(text: &str, point: Point) -> PointUtf16 {
        let line_start = row_start(text, point.row);
        let scan_end = (line_start + point.column as usize).min(text.len());
        let column = text[line_start..scan_end]
            .chars()
            .map(|ch| ch.len_utf16() as u32)
            .sum();
        PointUtf16::new(point.row, column)
    }

    fn point_utf16_to_point(text: &str, target: PointUtf16) -> Point {
        Point::new(target.row, line_column_bytes(text, target))
    }

    fn point_utf16_to_offset(text: &str, target: PointUtf16) -> usize {
        row_start(text, target.row) + line_column_bytes(text, target) as usize
    }

    /// Byte offset of the first character of `row`.
    fn row_start(text: &str, row: u32) -> usize {
        if row == 0 {
            return 0;
        }
        text.match_indices('\n')
            .nth(row as usize - 1)
            .map(|(ix, _)| ix + 1)
            .unwrap_or(text.len())
    }

    /// Byte column reached by advancing `target.column` UTF-16 units into
    /// `target.row`, stopping at the end of the line.
    fn line_column_bytes(text: &str, target: PointUtf16) -> u32 {
        let line = &text[row_start(text, target.row)..];
        let mut utf16 = 0u32;
        let mut bytes = 0u32;
        for ch in line.chars() {
            if ch == '\n' || utf16 >= target.column {
                break;
            }
            utf16 += ch.len_utf16() as u32;
            bytes += ch.len_utf8() as u32;
        }
        bytes
    }

    /// Characters spanning every UTF-8 width, plus the newlines that make rows
    /// and the chunk boundaries interesting.
    const ALPHABET: [char; 10] = ['a', 'z', '\n', '\n', 'é', 'ß', '世', '界', '𝄞', '🎉'];

    fn sample(seed: &mut u64, len: usize) -> String {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        };
        (0..len)
            .map(|_| ALPHABET[(next() as usize) % ALPHABET.len()])
            .collect()
    }

    #[test]
    fn the_rope_agrees_with_a_char_walk_over_random_text() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for case in 0..40 {
            let text = sample(&mut seed, 1 + case * 3);
            let rope = Rope::from(text.as_str());

            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) {
                    continue;
                }
                assert_eq!(
                    rope.offset_to_offset_utf16(offset),
                    offset_to_offset_utf16(&text, offset),
                    "offset_to_offset_utf16 at {offset} of {text:?}"
                );
                assert_eq!(
                    rope.offset_to_point_utf16(offset),
                    offset_to_point_utf16(&text, offset),
                    "offset_to_point_utf16 at {offset} of {text:?}"
                );
            }

            let len_utf16 = offset_to_offset_utf16(&text, text.len()).0;
            for unit in 0..=len_utf16 {
                assert_eq!(
                    rope.offset_utf16_to_offset(OffsetUtf16(unit)),
                    offset_utf16_to_offset(&text, OffsetUtf16(unit)),
                    "offset_utf16_to_offset at {unit} of {text:?}"
                );
            }

            let rows = text.matches('\n').count() as u32;
            for row in 0..=rows {
                let line_bytes = text[row_start(&text, row)..]
                    .split('\n')
                    .next()
                    .map(str::len)
                    .unwrap_or(0);
                for column in 0..=(line_bytes as u32 + 2) {
                    let point = Point::new(row, column);
                    if column as usize <= line_bytes
                        && text.is_char_boundary(row_start(&text, row) + column as usize)
                    {
                        assert_eq!(
                            rope.point_to_point_utf16(point),
                            point_to_point_utf16(&text, point),
                            "point_to_point_utf16 at {point:?} of {text:?}"
                        );
                    }
                    let target = PointUtf16::new(row, column);
                    assert_eq!(
                        rope.point_utf16_to_point(target),
                        point_utf16_to_point(&text, target),
                        "point_utf16_to_point at {target:?} of {text:?}"
                    );
                    assert_eq!(
                        rope.point_utf16_to_offset(target),
                        point_utf16_to_offset(&text, target),
                        "point_utf16_to_offset at {target:?} of {text:?}"
                    );
                }
            }
        }
    }
}

/// Reference implementations of the row and clipping primitives, and a
/// randomized check that the rope agrees with them.
///
/// The rope answers these from chunk bitmaps in a single tree descent, which
/// has to stay correct for a row spanning several chunks and for a column that
/// is not a character boundary. These walk the text directly instead.
#[cfg(test)]
mod clip_reference {
    use super::*;

    fn row_byte_range(text: &str, row: u32) -> Range<usize> {
        let mut start = 0usize;
        for _ in 0..row {
            match text[start..].find('\n') {
                Some(ix) => start += ix + 1,
                None => return text.len()..text.len(),
            }
        }
        let end = text[start..]
            .find('\n')
            .map(|ix| start + ix)
            .unwrap_or(text.len());
        start..end
    }

    fn line_len(text: &str, row: u32) -> u32 {
        let rows = text.matches('\n').count() as u32;
        if row > rows {
            return 0;
        }
        let range = row_byte_range(text, row);
        (range.end - range.start) as u32
    }

    fn clip_point(text: &str, point: Point, bias: Bias) -> Point {
        let rows = text.matches('\n').count() as u32;
        if point.row > rows {
            return Point::new(rows, line_len(text, rows));
        }
        let range = row_byte_range(text, point.row);
        let len = (range.end - range.start) as u32;
        let mut column = point.column.min(len) as usize;
        match bias {
            Bias::Left => {
                while column > 0 && !text.is_char_boundary(range.start + column) {
                    column -= 1;
                }
            },
            Bias::Right => {
                while column < len as usize && !text.is_char_boundary(range.start + column) {
                    column += 1;
                }
            },
        }
        Point::new(point.row, column as u32)
    }

    /// Columns are counted in UTF-16 code units, so the clip converts into
    /// bytes, clips there, and converts back the way the rope's composition of
    /// the three conversions does.
    fn clip_point_utf16(text: &str, point: PointUtf16, bias: Bias) -> PointUtf16 {
        let rows = text.matches('\n').count() as u32;
        let row = point.row.min(rows);
        let range = row_byte_range(text, row);
        let line = &text[range.clone()];

        let mut utf16 = 0u32;
        let mut bytes = 0usize;
        for ch in line.chars() {
            if utf16 >= point.column {
                break;
            }
            utf16 += ch.len_utf16() as u32;
            bytes += ch.len_utf8();
        }
        let byte_column = if point.row > rows { line.len() } else { bytes };

        let clipped = clip_point(text, Point::new(row, byte_column as u32), bias);
        let column = line[..clipped.column as usize]
            .chars()
            .map(|ch| ch.len_utf16() as u32)
            .sum();
        PointUtf16::new(row, column)
    }

    const ALPHABET: [char; 8] = ['a', 'b', '\n', '\n', 'é', '世', '𝄞', '🎉'];

    fn sample(seed: &mut u64, len: usize) -> String {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        };
        (0..len)
            .map(|_| ALPHABET[(next() as usize) % ALPHABET.len()])
            .collect()
    }

    #[test]
    fn the_rope_agrees_with_a_text_walk_over_random_ropes() {
        let mut seed = 0x1234_5678_9abc_def0_u64;
        for case in 0..30 {
            let text = sample(&mut seed, 1 + case * 4);
            let rope = Rope::from(text.as_str());
            let rows = text.matches('\n').count() as u32;

            for row in 0..=rows + 1 {
                assert_eq!(
                    rope.line_len(row),
                    line_len(&text, row),
                    "line_len at row {row} of {text:?}"
                );
                assert_eq!(
                    rope.row_byte_range(row),
                    row_byte_range(&text, row),
                    "row_byte_range at row {row} of {text:?}"
                );

                let len = line_len(&text, row);
                for column in 0..=len + 3 {
                    for bias in [Bias::Left, Bias::Right] {
                        let point = Point::new(row, column);
                        assert_eq!(
                            rope.clip_point(point, bias),
                            clip_point(&text, point, bias),
                            "clip_point at {point:?} {bias:?} of {text:?}"
                        );
                        let utf16 = PointUtf16::new(row, column);
                        assert_eq!(
                            rope.clip_point_utf16(utf16, bias),
                            clip_point_utf16(&text, utf16, bias),
                            "clip_point_utf16 at {utf16:?} {bias:?} of {text:?}"
                        );
                    }
                }
            }
        }
    }
}

/// The streaming line walk against the per-row path it replaces.
///
/// The walk carries one cursor across chunk boundaries, so what can go wrong
/// is a row whose newline sits at a chunk's last byte, a row longer than a
/// chunk, or the last row of a rope with no trailing newline.
#[cfg(test)]
mod line_walk_tests {
    use super::*;

    /// Every row's byte length and text, read the per-row way.
    fn per_row(rope: &Rope) -> (Vec<u32>, Vec<String>) {
        let rows = rope.max_point().row;
        let lens = (0..=rows).map(|row| rope.line_len(row)).collect();
        let texts = (0..=rows)
            .map(|row| rope.chunks_in_line(row).collect::<String>())
            .collect();
        (lens, texts)
    }

    fn walked(rope: &Rope, rows: Range<u32>) -> (Vec<u32>, Vec<String>) {
        let mut lens = Vec::new();
        let mut walk = rope.line_walk(rows.clone());
        while let Some((_, len)) = walk.next_len() {
            lens.push(len);
        }

        let mut texts = Vec::new();
        let mut walk = rope.line_walk(rows);
        let mut scratch = String::new();
        loop {
            scratch.clear();
            match walk.next_into(&mut scratch) {
                Some(_) => texts.push(scratch.clone()),
                None => break,
            }
        }
        (lens, texts)
    }

    fn check(text: &str) {
        let rope = Rope::from(text);
        let rows = rope.max_point().row;
        let (want_lens, want_texts) = per_row(&rope);
        let (got_lens, got_texts) = walked(&rope, 0..rows + 1);

        assert_eq!(got_lens, want_lens, "row lengths of {text:?}");
        assert_eq!(got_texts, want_texts, "row texts of {text:?}");
        assert_eq!(
            rope.line_lens_in_range(0..rows + 3),
            want_lens.iter().copied().chain([0, 0]).collect::<Vec<_>>(),
            "line_lens_in_range past the end of {text:?}"
        );
    }

    #[test]
    fn a_walk_matches_the_per_row_path() {
        let long = "x".repeat(MAX_BASE * 3);
        for text in [
            "",
            "\n",
            "a",
            "a\n",
            "a\nb",
            "a\nb\n",
            "\n\n\n",
            "a\n\nb\n\nc",
            &long,
            &format!("{long}\n{long}"),
            &format!("a\n{long}\n\nb"),
            // A newline landing exactly on a chunk boundary, which is where a
            // walk that forgets to advance its cursor repeats a row.
            &format!("{}\n{}", "y".repeat(MAX_BASE), "z".repeat(MAX_BASE)),
            &format!("{}\nz", "y".repeat(MAX_BASE - 1)),
            "héllo\nwörld\n𝄞\n🎉a",
        ] {
            check(text);
        }
    }

    #[test]
    fn a_walk_starts_at_the_row_it_is_given() {
        let text = "zero\none\ntwo\nthree\nfour";
        let rope = Rope::from(text);
        let mut walk = rope.line_walk(2..4);

        let mut scratch = String::new();
        walk.next_into(&mut scratch).expect("row two");
        assert_eq!(scratch, "two");

        assert_eq!(
            walk.next_len(),
            Some((3, 5)),
            "row three's index and length"
        );
        assert_eq!(walk.next_len(), None, "the walk stops at the end row");
    }
}

/// Chunk-count drift under repeated edits.
///
/// Every edit rebuilds the rope from a prefix, the new text, and a suffix, and
/// the suffix begins wherever the cursor stopped. Without merging at that seam
/// the count climbs with the number of edits rather than with the size of the
/// text, and it never recovers.
#[cfg(test)]
mod chunk_density_tests {
    use super::*;

    fn chunk_count(rope: &Rope) -> usize {
        rope.chunks.iter().count()
    }

    /// The seam is worth merging when either side is short, and an edit only
    /// exercises one of those. Here the incoming chunk is not short, so the
    /// tail's own shortfall is the only thing that can trigger the merge.
    #[test]
    fn a_short_tail_absorbs_an_incoming_chunk_that_fits() {
        let mut rope = Rope::from("a".repeat(MAX_BASE + 1).as_str());
        assert_eq!(chunk_count(&rope), 2, "a full chunk and a one-byte tail");

        rope.append(Rope::from("b".repeat(MIN_BASE).as_str()));
        assert_eq!(
            chunk_count(&rope),
            2,
            "the incoming chunk fits in the tail, so it joins it rather than following it"
        );
    }

    #[test]
    fn scattered_replaces_leave_the_chunk_count_near_the_text() {
        let mut seed = 0x5deece66d_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let mut rope = Rope::from("abcdefghij".repeat(200).as_str());
        for _ in 0..300 {
            let len = rope.len();
            let at = rope.clip_offset((next() as usize) % (len + 1), Bias::Left);
            let end = rope.clip_offset((at + (next() as usize) % 6).min(len), Bias::Right);
            rope.replace(at..end, "xy");
        }

        let floor = rope.len().div_ceil(MAX_BASE);
        assert!(
            chunk_count(&rope) <= floor * 3 / 2,
            "after 300 edits over {} bytes the rope holds {} chunks, against {floor} for the text",
            rope.len(),
            chunk_count(&rope),
        );
    }
}

/// Whether a rope's summary depends on how its text got chunked.
///
/// A caller that treats unequal summaries as proof of unequal text is only
/// right if identical text always summarises identically. The combine has a
/// tie-break on the longest row and branches on whether a side spans rows, and
/// chunk boundaries differ widely between a rope built in one push, one built
/// in pieces, and one arrived at through edits.
#[cfg(test)]
mod summary_identity_tests {
    use super::*;

    /// The same text, reached four ways that chunk it differently.
    fn built_every_way(text: &str) -> Vec<(&'static str, Rope)> {
        let one_push = Rope::from(text);

        let mut in_pieces = Rope::new();
        for piece in text.as_bytes().chunks(3) {
            in_pieces.push(std::str::from_utf8(piece).expect("ascii fixture"));
        }

        let mut appended = Rope::new();
        for piece in text.as_bytes().chunks(MAX_BASE + 1) {
            appended.append(Rope::from(
                std::str::from_utf8(piece).expect("ascii fixture"),
            ));
        }

        // Reached by editing. Each marker goes in and straight back out, so the
        // text is what it started as while the chunking has been churned.
        let mut edited = Rope::from(text);
        for at in (0..=text.len()).step_by(5) {
            edited.replace(at..at, "@@@");
            edited.replace(at..at + 3, "");
        }

        vec![
            ("one push", one_push),
            ("in pieces", in_pieces),
            ("appended", appended),
            ("edited", edited),
        ]
    }

    #[test]
    fn the_same_text_summarises_the_same_however_it_was_built() {
        // Some fixtures land on the same layout whichever way they are built,
        // since appending merges seams. At least one has to actually differ, or
        // this compares summaries of identically chunked ropes and proves
        // nothing about chunking.
        let mut laid_out_differently = false;

        for text in [
            "",
            "a",
            "no newlines at all, just one long row of text here",
            "short\nrows\nof\nvarying\nlength\nhere\n",
            // Two rows of equal length, which is what the longest-row tie-break
            // decides between.
            "equalrow\nequalrow\nshort\n",
            "trailing newline\n",
            "\n\n\nleading blanks\n",
            &"filler line that runs past a chunk\n".repeat(9),
        ] {
            let built = built_every_way(text);
            for (label, rope) in &built {
                assert_eq!(
                    rope.to_string(),
                    text,
                    "{label} must hold the fixture's text"
                );
            }
            let layouts: Vec<Vec<usize>> = built
                .iter()
                .map(|(_, rope)| rope.chunks.iter().map(|c| c.text.len()).collect())
                .collect();
            laid_out_differently |= layouts.iter().any(|l| *l != layouts[0]);

            let (_, first) = &built[0];
            for (label, rope) in &built[1..] {
                assert_eq!(
                    rope.summary(),
                    first.summary(),
                    "{label} summarises {text:?} differently from one push"
                );
            }
        }

        assert!(
            laid_out_differently,
            "no fixture reached two different chunk layouts, so nothing here tested chunking"
        );
    }
}

/// Matching over the rope's chunks has to agree with matching over the same
/// text in one piece.
///
/// The oracle is the very same engine given the text as a single chunk, which
/// `regex_cursor` supports for `&str`, so the only thing differing between the
/// two runs is the chunking.
#[cfg(test)]
mod regex_cursor_tests {
    use super::Rope;
    use regex_cursor::{engines::meta::Regex, Input};

    /// Long enough that its matches land either side of a chunk boundary, and
    /// varied enough that they do not all sit at the same place within one.
    fn straddling() -> String {
        let mut text = String::new();
        for row in 0..40 {
            text.push_str(&format!("row {row}: alpha beta gamma\n"));
            text.push_str("    needle in the haystack\n");
            text.push_str("caf\u{e9} \u{4e2d}\u{6587} tail\n");
        }
        text
    }

    fn spans(regex: &Regex, input: Input<impl regex_cursor::Cursor>) -> Vec<(usize, usize)> {
        regex
            .find_iter(input)
            .map(|m| (m.start(), m.end()))
            .collect()
    }

    fn assert_agrees(pattern: &str, text: &str) {
        let regex = Regex::new(pattern).expect("the pattern compiles");
        let rope = Rope::from(text);

        assert_eq!(
            spans(&regex, rope.regex_input(0..rope.len())),
            spans(&regex, Input::new(text)),
            "chunked and whole-text matches differ for {pattern:?}"
        );
    }

    #[test]
    fn matching_over_chunks_agrees_with_matching_the_whole_text() {
        let text = straddling();
        assert!(
            Rope::from(text.as_str()).chunks().count() > 1,
            "the fixture has to span chunks for any of this to mean anything"
        );

        for pattern in [
            "needle",
            "haystack",
            "alpha beta gamma",
            "row [0-9]+",
            "caf.",
            "\u{4e2d}\u{6587}",
            "^caf\u{e9}",
            "tail$",
            "(?s)needle.{0,40}caf",
            r"\bbeta\b",
            "z+",
        ] {
            assert_agrees(pattern, &text);
        }
    }

    #[test]
    fn a_search_restricted_to_a_range_reports_rope_offsets() {
        let text = straddling();
        let rope = Rope::from(text.as_str());
        let regex = Regex::new("needle").expect("the pattern compiles");

        let whole = spans(&regex, rope.regex_input(0..rope.len()));
        assert!(whole.len() > 4, "the fixture has to have several matches");

        let from = whole[2].0;
        let found = spans(&regex, rope.regex_input(from..rope.len()));

        assert_eq!(
            found,
            whole[2..],
            "a range restricts which matches are found without moving them"
        );
    }

    #[test]
    fn an_empty_rope_has_one_empty_chunk_and_no_matches() {
        let rope = Rope::new();
        let regex = Regex::new("needle").expect("the pattern compiles");

        assert_eq!(spans(&regex, rope.regex_input(0..0)), Vec::new());
    }

    #[test]
    fn stepping_off_either_end_leaves_the_chunk_alone() {
        use regex_cursor::Cursor;

        let rope = Rope::from(straddling().as_str());
        let mut cursor = rope.regex_cursor();

        let first = cursor.chunk().to_vec();
        assert!(!cursor.backtrack(), "nothing precedes the first chunk");
        assert_eq!(cursor.chunk(), first, "and the failed step moved nothing");

        while cursor.advance() {}
        let last = cursor.chunk().to_vec();
        assert!(!cursor.advance(), "nothing follows the last chunk");
        assert_eq!(cursor.chunk(), last, "and the failed step moved nothing");

        assert_eq!(
            cursor.offset() + last.len(),
            rope.len(),
            "the last chunk ends where the rope does"
        );
    }
}
