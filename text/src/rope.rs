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
    /// One set bit per tab byte.
    ///
    /// A tab is the one character whose width depends on where it sits, so a
    /// caller measuring display columns has to find them before it can do
    /// anything cheaper than walking.
    tabs: Bitmap,
    /// One set bit per byte that occupies exactly one display cell on its own,
    /// which is the printable ASCII range.
    ///
    /// Where these cover a run, its byte length is its column width, so it can
    /// be measured without being decoded. Everything else -- tabs, control
    /// bytes, anything multi-byte -- is left clear, since its width is either
    /// positional or needs the character to answer.
    single_width: Bitmap,
    text: ArrayString<MAX_BASE>,
}

impl Chunk {
    fn new(text: &str) -> Self {
        let maps = chunk_bitmaps(text);
        let mut arr = ArrayString::new();
        arr.push_str(text);

        Self {
            chars: maps.chars,
            chars_utf16: maps.chars_utf16,
            newlines: maps.newlines,
            tabs: maps.tabs,
            single_width: maps.single_width,
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
        let maps = chunk_bitmaps(s);
        self.chars |= maps.chars << offset;
        self.chars_utf16 |= maps.chars_utf16 << offset;
        self.newlines |= maps.newlines << offset;
        self.tabs |= maps.tabs << offset;
        self.single_width |= maps.single_width << offset;
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
        let mut bytes = self.advance_utf16(row.start, point.column as usize, row.end);

        // A column landing inside a character comes back rounded up to its end,
        // so what reaches `clip_point` below is already a boundary and the bias
        // has nothing left to decide. Consuming more code units than the column
        // asked for is how that shows. Left has to answer with the character's
        // start instead, since a Left clip that moves rightward would put an
        // LSP position a whole character past the one it named.
        if matches!(bias, Bias::Left)
            && bits_in(self.chars_utf16, row.start..bytes).count_ones() > point.column
        {
            let starts = bits_in(self.chars, row.start..bytes);
            if starts != 0 {
                bytes = row.start + (Bitmap::BITS - 1 - starts.leading_zeros()) as usize;
            }
        }

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

    /// Bytes from `start` to where advancing `column` bytes lands, stopping at
    /// the end of the line `start` falls on.
    ///
    /// The byte counterpart of [`Self::line_column_bytes`]. A column past the
    /// row's end names no position in that row, so it collapses onto the end
    /// rather than continuing into the row below.
    fn line_column_capped(&self, start: usize, column: u32) -> u32 {
        let line_end = self.line_end_from(start);
        (line_end.min(start + column as usize) - start.min(line_end)) as u32
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

    /// The widest row in this chunk, as `(row, chars)`.
    ///
    /// Walks the rows in order and keeps the first of any tie, which is what
    /// [`TextSummary::from_str`] does. The two have to agree: a summary is a
    /// property of the text, and which of them produced it depends only on how
    /// the text happened to be chunked.
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

        if last_line_chars > best_chars {
            best_row = total_rows;
            best_chars = last_line_chars;
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

    /// Append `text`, topping up the last chunk before opening new ones.
    ///
    /// The tail is filled to the brim only when `text` fits in what is left of
    /// it. Otherwise it is topped up to [`MIN_BASE`] and no further, so what
    /// remains is over half a chunk rather than the crumb a brim-full tail
    /// would leave. That crumb is what strands: the caller appends more chunks
    /// after it, and nothing merges a chunk that is no longer the last.
    pub fn push(&mut self, mut text: &str) {
        let mut consumed = 0usize;
        self.chunks.update_last(
            |last_chunk| {
                if text.is_empty() {
                    return;
                }
                let take = if last_chunk.text.len() + text.len() <= MAX_BASE {
                    text.len()
                } else {
                    // Rounded up, since stopping short of a boundary would put
                    // the tail back under MIN_BASE.
                    let mut take =
                        cmp::min(MIN_BASE.saturating_sub(last_chunk.text.len()), text.len());
                    while !text.is_char_boundary(take) {
                        take += 1;
                    }
                    take
                };
                if take > 0 {
                    last_chunk.push_str(&text[..take]);
                    consumed = take;
                }
            },
            (),
        );
        text = &text[consumed..];
        if text.is_empty() {
            return;
        }

        // Split the whole remainder first, then hand it over in one go. Pushing
        // a chunk at a time walks the rightmost spine for each one, which over a
        // file-sized append is a walk per 128 bytes, where extending builds the
        // leaves bottom-up and joins them with a single append.
        let mut chunks = Vec::with_capacity(text.len().div_ceil(MAX_BASE));
        while !text.is_empty() {
            let mut split_ix = cmp::min(MAX_BASE, text.len());
            while !text.is_char_boundary(split_ix) {
                split_ix -= 1;
            }
            let (chunk, remainder) = text.split_at(split_ix);
            chunks.push(chunk);
            text = remainder;
        }
        self.chunks.extend(chunks.into_iter().map(Chunk::new), ());
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

    /// Panic unless every chunk but the last carries at least [`MIN_BASE`]
    /// bytes.
    ///
    /// The last is exempt because it is the one a later push can still top up.
    /// Any other short chunk is stranded, and the count then tracks how many
    /// edits were made rather than how much text there is, which deepens the
    /// tree for every descent through it.
    ///
    /// Three bytes of slack, since a split rounds up to a character boundary
    /// and a character is at most four bytes wide.
    #[cfg(test)]
    fn assert_chunks_dense(&self) {
        let mut chunks = self.chunks.iter().peekable();
        let mut row = 0;
        while let Some(chunk) = chunks.next() {
            if chunks.peek().is_some() {
                assert!(
                    chunk.text.len() + 3 >= MIN_BASE,
                    "chunk {row} of {} holds {} bytes, under the {MIN_BASE} floor",
                    self.chunks.iter().count(),
                    chunk.text.len(),
                );
            }
            row += 1;
        }
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

    /// Summarize the text `range` covers.
    ///
    /// An empty or inverted range covers no text and so answers the default
    /// summary, as every form of the chunk walk yields nothing for one. Two
    /// independently derived offsets sometimes arrive out of order, and
    /// answering that with a panic makes it a crash in a caller that only asked
    /// a question.
    pub fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        let mut cursor = self.cursor(range.start);
        cursor.summary(range.end)
    }

    pub fn max_point(&self) -> Point {
        self.chunks.summary().lines
    }

    /// Byte offset of `target`.
    ///
    /// A row past the last one answers the rope's length, and a column past its
    /// row's end answers that row's end. Clamping rather than running on is
    /// what keeps this the inverse of [`Self::offset_to_point`] and keeps it
    /// agreeing with [`Self::clip_point`] and the UTF-16 conversions.
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
        let (line_start, column) = if remaining_rows == 0 {
            (0, target.column - chunk_start_point.column)
        } else {
            (
                nth_newline_offset_bitmap(chunk.newlines, remaining_rows),
                target.column,
            )
        };

        chunk_start_offset + line_start + chunk.line_column_capped(line_start, column) as usize
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

    /// Convert every offset to a [`Point`], in one forward walk of the tree.
    ///
    /// The walk only goes forward, so the offsets have to be visited in
    /// ascending order. Input already in that order is visited as it stands,
    /// which is what every caller that resolves a sorted set hands over, and
    /// only input that is actually out of order pays for a permutation.
    pub fn offsets_to_points_batch(&self, offsets: &[usize]) -> Vec<Point> {
        let ascending_order: Option<Vec<usize>> = (!offsets.is_sorted()).then(|| {
            let mut order: Vec<usize> = (0..offsets.len()).collect();
            order.sort_unstable_by_key(|&i| offsets[i]);
            order
        });

        let mut results = vec![Point::zero(); offsets.len()];
        let mut cursor = self.chunks.cursor::<Dimensions<usize, Point>>(());
        let summary_lines = self.chunks.summary().lines;

        for step in 0..offsets.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let offset = offsets[original_idx];
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
                    let (line_start, column) = if remaining_rows == 0 {
                        (0, point.column - chunk_start_point.column)
                    } else {
                        (
                            nth_newline_offset_bitmap(chunk.newlines, remaining_rows),
                            point.column,
                        )
                    };

                    chunk_start_offset
                        + line_start
                        + chunk.line_column_capped(line_start, column) as usize
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
            last_row: self.max_point().row,
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

    /// One row's chunks, each saying whether its column width is its byte
    /// length. See [`Self::measured_chunks_in_range`].
    pub fn measured_chunks_in_line(&self, row: u32) -> MeasuredChunksInRange<'_> {
        let range = self.row_byte_range(row);
        self.measured_chunks_in_range(range)
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
        let (chunk_start_offset, _end, chunk_opt) =
            self.chunks.find::<usize, _>((), &offset, Bias::Right);
        let chunk = match chunk_opt {
            Some(c) => c,
            None => return true,
        };
        let local = offset - chunk_start_offset;
        chunk.text.as_str().is_char_boundary(local)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        let offset = offset.min(self.len());
        if offset == 0 {
            return offset;
        }

        // One descent answers both questions. Asking whether the offset is on a
        // boundary and then clipping it are the same lookup, and this runs per
        // selection per keypress through the grapheme steppers.
        let (chunk_start_offset, _end, chunk_opt) =
            self.chunks.find::<usize, _>((), &offset, Bias::Right);
        // No chunk holds the offset only when it is the rope's end, which the
        // clamp above already put it at and which is always a boundary.
        let Some(chunk) = chunk_opt else {
            return offset;
        };

        clip_within(chunk.text.as_str(), chunk_start_offset, offset, bias)
    }

    /// The chunk holding `offset`, where that chunk starts, and `offset` clipped
    /// to a char boundary inside it.
    ///
    /// One descent for what the grapheme steppers otherwise take three of. They
    /// each want the chunk, the clipped offset, and then the chunk again to seed
    /// their cursor loop.
    ///
    /// `None` past the last chunk, which after the clamp is only the rope's end.
    fn chunk_clipped(&self, offset: usize, bias: Bias) -> Option<(&str, usize, usize)> {
        let offset = offset.min(self.len());
        let (chunk_start, _end, chunk) = self.chunks.find::<usize, _>((), &offset, Bias::Right);
        let text = chunk?.text.as_str();
        Some((
            text,
            chunk_start,
            clip_within(text, chunk_start, offset, bias),
        ))
    }

    /// Move `offset` to a grapheme-cluster boundary, or leave it where it is if
    /// it is already on one.
    ///
    /// `bias` picks the direction to escape a offset that has landed inside a
    /// cluster, `Left` to the boundary before it and `Right` to the one after.
    ///
    /// This is a clamp, not a step. The stepping pair moves off a boundary
    /// rather than staying on it, which is what a cursor motion wants and what
    /// snapping a range must not do. Applying a step to an already-aligned
    /// range would grow it by a cluster at each end.
    ///
    /// See also:
    /// - [`Self::next_grapheme_boundary`] and [`Self::prev_grapheme_boundary`] for the stepping
    ///   pair.
    pub fn clip_to_grapheme_boundary(&self, offset: usize, bias: Bias) -> usize {
        // One descent for the clip, the ASCII check, and the cursor loop, all
        // of which want the chunk holding the offset.
        let Some((first, first_start, offset)) = self.chunk_clipped(offset, Bias::Left) else {
            return offset.min(self.len());
        };
        if offset == 0 || offset >= self.len() {
            return offset;
        }

        if ascii_pair_breaks(first.as_bytes(), offset - first_start) {
            return offset;
        }

        // The cursor needs the text before `offset` to answer, since whether a
        // boundary exists here depends on what it would be splitting.
        let mut held = Some((first, first_start));
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        let on_boundary = loop {
            let Some((chunk, chunk_start)) = held.take().or_else(|| self.chunk_at(offset)) else {
                return offset;
            };
            match cursor.is_boundary(chunk, chunk_start) {
                Ok(answer) => break answer,
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let Some((ctx, ctx_start)) = self.chunk_ending_at(end) else {
                        return offset;
                    };
                    cursor.provide_context(ctx, ctx_start);
                },
                // `is_boundary` asks only for pre-context, or rejects the
                // offset outright. It never asks for a neighbouring chunk, so
                // there is nothing to step to and no answer to be had. The
                // stepping pair goes through `next_boundary` and
                // `prev_boundary`, which do ask, and handle it themselves.
                Err(_) => return offset,
            }
        };

        if on_boundary {
            return offset;
        }
        match bias {
            Bias::Left => self.prev_grapheme_boundary(offset),
            Bias::Right => self.next_grapheme_boundary(offset),
        }
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
    /// - [`Self::clip_to_grapheme_boundary`] to snap onto one instead of past it.
    pub fn next_grapheme_boundary(&self, offset: usize) -> usize {
        // The one descent this whole step takes, unless a cluster runs past the
        // chunk. It serves the fast path below, the clip, and the first turn of
        // the cursor loop, which all want the chunk holding `offset`.
        let Some((first, first_start, clipped)) = self.chunk_clipped(offset, Bias::Left) else {
            return offset.min(self.len());
        };

        // A cluster holding the ASCII scalar at `offset` reaches past it only
        // through Extend, ZWJ, SpacingMark or a regional indicator, none of
        // which is ASCII, or through CR before LF, which the check excludes.
        //
        // Read against the unclipped offset, since a byte under 0x80 is never a
        // continuation byte. An offset this accepts is on a char boundary
        // already, and one that is not always fails it.
        if ascii_pair_breaks(first.as_bytes(), offset + 1 - first_start) {
            return offset + 1;
        }

        let offset = clipped;
        if offset >= self.len() {
            return offset;
        }

        // The clip stays inside the chunk it was found in, so the loop opens on
        // the chunk already in hand and descends again only to cross a seam.
        let mut held = Some((first, first_start));
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        let mut pos = offset;
        loop {
            let Some((chunk, chunk_start)) = held.take().or_else(|| self.chunk_at(pos)) else {
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

    /// [`Self::prev_grapheme_boundary`] for every offset, in one forward walk
    /// of the tree.
    ///
    /// Answers exactly what the scalar call answers, offset for offset. A
    /// caller stepping a few hundred cursors back by a cluster would otherwise
    /// descend from the root for each of them.
    ///
    /// The walk only goes forward, so the offsets are visited in ascending
    /// order and only input that is actually out of order pays for a
    /// permutation. An offset the walk cannot settle from the chunk in hand --
    /// a cluster reaching back past the chunk start, or an offset that is not
    /// on a char boundary -- falls back to the scalar call.
    pub fn prev_grapheme_boundaries_batch(&self, offsets: &[usize]) -> Vec<usize> {
        let ascending_order: Option<Vec<usize>> = (!offsets.is_sorted()).then(|| {
            let mut order: Vec<usize> = (0..offsets.len()).collect();
            order.sort_unstable_by_key(|&i| offsets[i]);
            order
        });

        let mut results = vec![0usize; offsets.len()];
        let mut cursor = self.chunks.cursor::<usize>(());
        let len = self.len();

        for step in 0..offsets.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let offset = offsets[original_idx];
            if offset == 0 {
                continue;
            }

            // The cluster ends at `offset`, so the chunk that decides it is the
            // one holding the byte before it.
            cursor.seek_forward(&(offset - 1), Bias::Right);
            let chunk_start = *cursor.start();
            let settled = cursor.item().and_then(|chunk| {
                let text = chunk.text.as_str();
                let local = offset.checked_sub(chunk_start)?;
                if local > text.len() || !text.is_char_boundary(local) {
                    return None;
                }
                match GraphemeCursor::new(offset, len, true).prev_boundary(text, chunk_start) {
                    Ok(Some(boundary)) => Some(boundary),
                    _ => None,
                }
            });

            results[original_idx] = match settled {
                Some(boundary) => boundary,
                None => self.prev_grapheme_boundary(offset),
            };
        }
        results
    }

    /// [`Self::next_grapheme_boundary`] for every offset, in one forward walk
    /// of the tree.
    ///
    /// Forward mirror of [`Self::prev_grapheme_boundaries_batch`], with the
    /// same equivalence to the scalar call and the same fallback to it for an
    /// offset the chunk in hand leaves unsettled.
    pub fn next_grapheme_boundaries_batch(&self, offsets: &[usize]) -> Vec<usize> {
        let ascending_order: Option<Vec<usize>> = (!offsets.is_sorted()).then(|| {
            let mut order: Vec<usize> = (0..offsets.len()).collect();
            order.sort_unstable_by_key(|&i| offsets[i]);
            order
        });

        let mut results = vec![0usize; offsets.len()];
        let mut cursor = self.chunks.cursor::<usize>(());
        let len = self.len();

        for step in 0..offsets.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let offset = offsets[original_idx];

            cursor.seek_forward(&offset, Bias::Right);
            let chunk_start = *cursor.start();
            let settled = cursor.item().and_then(|chunk| {
                let text = chunk.text.as_str();
                let local = offset.checked_sub(chunk_start)?;
                if local > text.len() || !text.is_char_boundary(local) {
                    return None;
                }
                if ascii_pair_breaks(text.as_bytes(), local + 1) {
                    return Some(offset + 1);
                }
                match GraphemeCursor::new(offset, len, true).next_boundary(text, chunk_start) {
                    Ok(Some(boundary)) => Some(boundary),
                    _ => None,
                }
            });

            results[original_idx] = match settled {
                Some(boundary) => boundary,
                None => self.next_grapheme_boundary(offset),
            };
        }
        results
    }

    /// [`Self::clip_to_grapheme_boundary`] for every request, in one forward
    /// walk of the tree.
    ///
    /// Answers exactly what the scalar call answers, request for request. A
    /// caller snapping both endpoints of a few hundred selections otherwise
    /// descends from the root twice per selection.
    ///
    /// One walk serves both biases. The clip's own pre-step is always
    /// [`Bias::Left`], and the bias picks only which way an offset that has
    /// landed inside a cluster escapes it. So the walk decides on-boundary for
    /// every request, and only the offsets inside a cluster are split by bias
    /// and stepped through the two directional batches.
    ///
    /// The walk only goes forward, so the requests are visited in ascending
    /// offset order and only input that is actually out of order pays for a
    /// permutation. A request the chunk in hand leaves unsettled falls back to
    /// the scalar call.
    pub fn clip_to_grapheme_boundaries_batch(&self, requests: &[(usize, Bias)]) -> Vec<usize> {
        let ascending_order: Option<Vec<usize>> =
            (!requests.is_sorted_by_key(|&(offset, _)| offset)).then(|| {
                let mut order: Vec<usize> = (0..requests.len()).collect();
                order.sort_unstable_by_key(|&i| requests[i].0);
                order
            });

        let mut results = vec![0usize; requests.len()];
        let mut left_escapes: Vec<usize> = Vec::new();
        let mut right_escapes: Vec<usize> = Vec::new();
        let mut cursor = self.chunks.cursor::<usize>(());
        let len = self.len();

        for step in 0..requests.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let (offset, bias) = requests[original_idx];

            cursor.seek_forward(&offset, Bias::Right);
            let chunk_start = *cursor.start();
            let on_boundary = cursor.item().and_then(|chunk| {
                let text = chunk.text.as_str();
                let local = offset.checked_sub(chunk_start)?;
                if local > text.len() || !text.is_char_boundary(local) {
                    return None;
                }
                if offset == 0 || offset >= len || ascii_pair_breaks(text.as_bytes(), local) {
                    return Some(true);
                }
                GraphemeCursor::new(offset, len, true)
                    .is_boundary(text, chunk_start)
                    .ok()
            });

            match on_boundary {
                Some(true) => results[original_idx] = offset,
                Some(false) => match bias {
                    Bias::Left => left_escapes.push(original_idx),
                    Bias::Right => right_escapes.push(original_idx),
                },
                None => results[original_idx] = self.clip_to_grapheme_boundary(offset, bias),
            }
        }

        for (escapes, boundaries) in [
            (
                &left_escapes,
                self.prev_grapheme_boundaries_batch(&escaped_offsets(requests, &left_escapes)),
            ),
            (
                &right_escapes,
                self.next_grapheme_boundaries_batch(&escaped_offsets(requests, &right_escapes)),
            ),
        ] {
            for (&original_idx, boundary) in escapes.iter().zip(boundaries) {
                results[original_idx] = boundary;
            }
        }
        results
    }

    /// The character at every offset, in one forward walk of the tree.
    ///
    /// For an offset on a char boundary, each answer is what
    /// `chars_at(offset).next()` would give, so one at or past the rope end
    /// reads as `None`. For a caller reading one character at each of many
    /// places, such as the cell under every block cursor, this replaces a root
    /// descent per offset with a single walk.
    ///
    /// Offsets are expected to sit on char boundaries. One that splits a scalar
    /// reads as `None` rather than being clipped onto the character it lands
    /// inside, since a caller asking about a position that is not a character
    /// has no character to be told about. That is also where the equivalence
    /// above stops, since `chars_at` slices from the split offset and panics.
    ///
    /// The walk only goes forward, so the offsets are visited in ascending
    /// order and only input that is actually out of order pays for a
    /// permutation.
    pub fn chars_at_batch(&self, offsets: &[usize]) -> Vec<Option<char>> {
        let ascending_order: Option<Vec<usize>> = (!offsets.is_sorted()).then(|| {
            let mut order: Vec<usize> = (0..offsets.len()).collect();
            order.sort_unstable_by_key(|&i| offsets[i]);
            order
        });

        let mut results = vec![None; offsets.len()];
        let mut cursor = self.chunks.cursor::<usize>(());

        for step in 0..offsets.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let offset = offsets[original_idx];
            cursor.seek_forward(&offset, Bias::Right);
            let chunk_start = *cursor.start();
            results[original_idx] = cursor.item().and_then(|chunk| {
                let local = offset.checked_sub(chunk_start)?;
                chunk.text.as_str().get(local..)?.chars().next()
            });
        }
        results
    }

    /// Offset of the first grapheme-cluster boundary before `offset`, or
    /// `offset` itself at the rope start.
    ///
    /// Backward mirror of [`Self::next_grapheme_boundary`], with the same
    /// cluster definition and the same left-clipping of an `offset` that is not
    /// on a char boundary.
    pub fn prev_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        if offset == 0 {
            return 0;
        }

        // Taken at `offset - 1` rather than at `offset`, so a step back from the
        // rope end, where nothing holds `offset`, still lands on a chunk. It
        // clips `offset` too. That offset either sits in this chunk, or is
        // exactly the chunk's end and so already a boundary.
        let Some((first, first_start)) = self.chunk_at(offset - 1) else {
            return offset;
        };

        // The mirror of the forward step's fast path, one position back.
        if ascii_pair_breaks(first.as_bytes(), offset - 1 - first_start) {
            return offset - 1;
        }

        let offset = clip_within(first, first_start, offset, Bias::Left);
        if offset == 0 {
            return 0;
        }

        // The clip only moves back within this chunk, so `offset - 1` is still
        // in it and the loop opens on the chunk already in hand.
        let mut held = Some((first, first_start));
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        let mut pos = offset - 1;
        loop {
            let Some((chunk, chunk_start)) = held.take().or_else(|| self.chunk_at(pos)) else {
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
        let (chunk_start, _end, chunk) = self.chunks.find::<usize, _>((), &offset, Bias::Right);
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

    /// The chunks spanning `range`, front to back, each clipped to it.
    ///
    /// A range that is empty or inverted covers no text and so yields nothing
    /// at all, rather than one empty chunk. Callers that compute a range from
    /// two independently derived offsets get an empty answer instead of a
    /// panic when the two arrive out of order.
    ///
    /// See also:
    /// - [`Self::reversed_chunks_in_range`] for the same span back to front.
    pub fn chunks_in_range(&self, range: Range<usize>) -> ChunksInRange<'_> {
        ChunksInRange {
            chunks: self.chunks_with_chunk(range),
        }
    }

    /// This rope's chunks over `range`, each paired with whether its column
    /// width is its byte length.
    ///
    /// For a caller measuring display columns, which can then add a run's
    /// length instead of decoding it. See [`MeasuredChunk`] for what the flag
    /// promises and what leaves it clear.
    pub fn measured_chunks_in_range(&self, range: Range<usize>) -> MeasuredChunksInRange<'_> {
        MeasuredChunksInRange {
            chunks: self.chunks_with_chunk(range),
        }
    }

    fn chunks_with_chunk(&self, range: Range<usize>) -> ChunksWithChunk<'_> {
        ChunksWithChunk {
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
        self.regex_cursor_over(0..self.len())
    }

    /// `span` of this rope's chunks as a haystack, presented as though nothing
    /// surrounded it.
    ///
    /// Offsets it reports are relative to `span`, and its first and last chunks
    /// are clipped to it, so the automata see the span's edges as the edges of
    /// the text. Most callers want [`Self::regex_slice_input`].
    pub fn regex_cursor_over(&self, span: Range<usize>) -> RegexChunks<'_> {
        let chunks = self.chunks_cursor_at(span.start);
        RegexChunks { chunks, span }
    }

    /// The chunks cursor parked on the chunk holding `at`.
    ///
    /// A cursor seeked past the last chunk holds no item, and a haystack has to
    /// sit on one, so it steps back onto the last.
    fn chunks_cursor_at(&self, at: usize) -> sum_tree::Cursor<'_, '_, Chunk, usize> {
        let mut chunks = self.chunks.cursor::<usize>(());
        chunks.seek(&at, Bias::Right);
        if chunks.item().is_none() {
            chunks.prev();
        }
        chunks
    }

    /// An [`Input`] searching `range` of this rope, with the rest of the rope
    /// still around it.
    ///
    /// The range says where matches may be found, not what the text is. A `^`
    /// at its start still asks whether a newline precedes it, and a word
    /// boundary still sees the character before, both of which a search
    /// resuming mid-buffer wants.
    ///
    /// Offsets come back as rope offsets either way.
    ///
    /// The haystack starts on the chunk holding `range.start` rather than on
    /// the rope's first. Every engine entry starts by moving to the range's
    /// start, and that move walks a chunk at a time from wherever the cursor
    /// sits, so a search resuming mid-buffer otherwise pays for every chunk
    /// ahead of it before matching anything.
    ///
    /// See also:
    /// - [`Self::regex_slice_input`] for when the range is the whole text.
    pub fn regex_input(&self, range: Range<usize>) -> Input<RegexChunks<'_>> {
        // The span stays the whole rope, which is what keeps the offsets rope
        // offsets and lets an assertion at the range's start read what precedes
        // it. `Input::new` seeds its own span from the cursor, and `range`
        // overwrites that outright.
        let haystack = RegexChunks {
            chunks: self.chunks_cursor_at(range.start),
            span: 0..self.len(),
        };
        Input::new(haystack).range(range)
    }

    /// An [`Input`] over `range` of this rope as though nothing surrounded it.
    ///
    /// For matching against a piece of text that happens to live in a buffer, a
    /// selection being the case in point. A `^` matches at the range's start
    /// unconditionally and a word boundary sees nothing before it, which is
    /// what the same text copied into a string would have given.
    ///
    /// **Offsets come back relative to `range`**, not to the rope, since to the
    /// automata the range is the whole text. Add `range.start` to place a match
    /// back in the buffer.
    ///
    /// See also:
    /// - [`Self::regex_input`] for searching part of a buffer, which reports rope offsets and lets
    ///   the surrounding text inform the assertions.
    pub fn regex_slice_input(&self, range: Range<usize>) -> Input<RegexChunks<'_>> {
        Input::new(self.regex_cursor_over(range))
    }

    /// The chunks spanning `range`, back to front, each clipped to it.
    ///
    /// The chunks arrive in reverse order but their text does not, so a caller
    /// rebuilding the span reverses the sequence rather than the bytes. An
    /// empty or inverted range yields nothing, as it does going forward.
    ///
    /// See also:
    /// - [`Self::chunks_in_range`] for the same span front to back.
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

        let remaining_rows = target.row - chunk_start_point.row;
        let line_start = if remaining_rows == 0 {
            0
        } else {
            nth_newline_offset_bitmap(chunk.newlines, remaining_rows)
        };

        let col_bytes = if remaining_rows == 0 {
            target.column - chunk_start_point.column
        } else {
            target.column
        };

        let scan_end = line_start + chunk.line_column_capped(line_start, col_bytes) as usize;
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

    /// The text `range` covers, as a rope of its own.
    ///
    /// An empty or inverted range covers no text and so answers the empty rope,
    /// for the reason [`Self::text_summary_for_range`] gives.
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

    /// The bytes spanning `range`, front to back.
    ///
    /// Walks [`Self::chunks_in_range`] and carries its contract, so an empty or
    /// inverted range yields no bytes.
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
    /// Index of the rope's last row.
    ///
    /// A start row past this one still resolves to a byte offset, because the
    /// conversion clamps, so the offset alone cannot tell the walk it has been
    /// asked for a row that does not exist.
    last_row: u32,
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
        if self.row >= self.end_row || self.row > self.last_row || self.offset > self.len {
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

/// A run of a rope's text, with what measuring it costs.
///
/// Yielded by [`Rope::measured_chunks_in_range`] for a caller working in
/// display columns.
pub struct MeasuredChunk<'a> {
    pub text: &'a str,
    /// Whether every byte here is one display cell on its own, so the run's
    /// column width is its byte length and no character in it need be decoded.
    ///
    /// False for a run holding a tab, whose width depends on where it starts,
    /// or anything outside printable ASCII, whose width the character has to
    /// answer for. Such a run is measured the long way.
    pub cell_per_byte: bool,
    /// Whether any byte here is a tab.
    ///
    /// For a caller that only needs to know whether the expensive character is
    /// present, such as one deciding whether a row's tab stops can move.
    pub has_tab: bool,
}

/// A rope's chunks over a range, each paired with whether it can be measured
/// by its length.
///
/// The flag comes off the chunk's own bitmap rather than a scan, so a caller
/// asking repeatedly about the same text pays for it once, when the chunk was
/// built.
pub struct MeasuredChunksInRange<'a> {
    chunks: ChunksWithChunk<'a>,
}

impl<'a> Iterator for MeasuredChunksInRange<'a> {
    type Item = MeasuredChunk<'a>;

    fn next(&mut self) -> Option<MeasuredChunk<'a>> {
        let (chunk, span) = self.chunks.next()?;
        let width = bits_in(chunk.single_width, span.clone());
        let covered = match span.len() as u32 >= Bitmap::BITS {
            true => !0,
            false => ((1 as Bitmap) << span.len()) - 1,
        };

        Some(MeasuredChunk {
            text: &chunk.text.as_str()[span.clone()],
            cell_per_byte: width == covered,
            has_tab: bits_in(chunk.tabs, span) != 0,
        })
    }
}

/// The chunks of a range, each with the byte span of it the range covers.
///
/// The shared walk under [`ChunksInRange`] and [`MeasuredChunksInRange`], which
/// differ only in what they report about each chunk.
struct ChunksWithChunk<'a> {
    chunks: sum_tree::Cursor<'a, 'a, Chunk, usize>,
    range: Range<usize>,
    started: bool,
}

impl<'a> Iterator for ChunksWithChunk<'a> {
    type Item = (&'a Chunk, Range<usize>);

    fn next(&mut self) -> Option<(&'a Chunk, Range<usize>)> {
        if self.range.start >= self.range.end {
            return None;
        }

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

        Some((chunk, local_start..local_end))
    }
}

pub struct ChunksInRange<'a> {
    chunks: ChunksWithChunk<'a>,
}

impl<'a> Iterator for ChunksInRange<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let (chunk, span) = self.chunks.next()?;
        Some(&chunk.text.as_str()[span])
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
    /// The rope span this cursor presents, which every chunk is clipped to and
    /// every offset is measured from.
    span: Range<usize>,
}

impl RegexChunks<'_> {
    /// Where the current chunk sits in the rope, clipped to the span.
    fn clipped(&self) -> Range<usize> {
        let Some(chunk) = self.chunks.item() else {
            return self.span.start..self.span.start;
        };
        let start = *self.chunks.start();
        let end = start + chunk.text.len();
        start.max(self.span.start)..end.min(self.span.end)
    }
}

impl RegexCursor for RegexChunks<'_> {
    fn chunk(&self) -> &[u8] {
        let Some(chunk) = self.chunks.item() else {
            return &[];
        };
        let start = *self.chunks.start();
        let clipped = self.clipped();
        &chunk.text.as_bytes()[clipped.start - start..clipped.end - start]
    }

    /// Chunks never split a codepoint, so every regex feature is available.
    fn utf8_aware(&self) -> bool {
        true
    }

    fn advance(&mut self) -> bool {
        // Peeked rather than stepped and undone, the trait requiring a failed
        // step to leave the chunk exactly where it was.
        let Some(next) = self.chunks.next_item() else {
            return false;
        };
        if self.clipped().end + next.text.len() <= self.span.start
            || self.clipped().end >= self.span.end
        {
            return false;
        }
        self.chunks.next();
        true
    }

    fn backtrack(&mut self) -> bool {
        // Stopping at the span keeps the contract rather than the answer. A
        // chunk before it clips to nothing, so stepping onto one would report a
        // move that left an empty chunk behind, which callers are entitled to
        // read as the collection being empty.
        if self.chunks.prev_item().is_none() || self.clipped().start <= self.span.start {
            return false;
        }
        self.chunks.prev();
        true
    }

    fn total_bytes(&self) -> Option<usize> {
        Some(self.span.len())
    }

    fn offset(&self) -> usize {
        self.clipped().start - self.span.start
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

    /// Summarize the text from the cursor to `end_offset`, and leave the cursor
    /// there.
    ///
    /// An `end_offset` at or below the cursor covers no text, so it answers the
    /// default summary and leaves the cursor put. Two independently derived
    /// offsets sometimes arrive out of order, and a cursor moved backward by one
    /// that covered nothing reads the next call from somewhere the caller never
    /// asked for.
    pub fn summary(&mut self, end_offset: usize) -> TextSummary {
        let mut result = TextSummary::default();
        if end_offset <= self.offset {
            return result;
        }

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

    /// The text from the cursor to `end_offset`, leaving the cursor there.
    ///
    /// An `end_offset` at or below the cursor covers no text, so it answers the
    /// empty rope and leaves the cursor put, for the reason
    /// [`Self::summary`] gives.
    pub fn slice(&mut self, end_offset: usize) -> Rope {
        let mut slice = Rope::new();
        if end_offset <= self.offset {
            return slice;
        }

        if let Some(chunk) = self.chunks.item() {
            let start_ix = self.offset - *self.chunks.start();
            let end_ix = end_offset.min(self.chunks.end()) - *self.chunks.start();
            if start_ix < end_ix {
                slice.push(&chunk.text[start_ix..end_ix]);
            }
        }

        if end_offset > self.chunks.end() {
            self.chunks.next();
            // Through `append` rather than straight onto the tree, so the
            // partial chunk pushed above merges with what follows it instead of
            // being stranded ahead of full chunks.
            slice.append(Rope {
                chunks: self.chunks.slice(&end_offset, Bias::Right),
            });

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

/// Whether the two scalars around `local` in `bytes` are both ASCII and break
/// between, which decides a grapheme boundary without a cursor.
///
/// Two adjacent scalars below 0x80 always break, save CR before LF. GB3 joins
/// that one pair, GB4 and GB5 break around the other controls, and every
/// remaining rule needs Hangul, Extend, ZWJ, SpacingMark, Prepend,
/// Extended_Pictographic, or a regional indicator, none of which any ASCII
/// scalar is. A byte below 0x80 is also never a UTF-8 continuation byte, so the
/// two reads identify whole scalars rather than the middle of one.
///
/// False at either end of `bytes`, which is how a caller holding one chunk
/// finds out that the answer lies across a seam. Reaching over it would cost
/// the descent this exists to skip, and every caller has a slower path that
/// answers a seam correctly.
/// The offsets `indices` names in `requests`, for a directional sub-pass of
/// [`Rope::clip_to_grapheme_boundaries_batch`].
fn escaped_offsets(requests: &[(usize, Bias)], indices: &[usize]) -> Vec<usize> {
    indices.iter().map(|&i| requests[i].0).collect()
}

/// `offset` moved to a char boundary, decided entirely inside `chunk`.
///
/// `chunk` must hold `offset` or end exactly at it. A chunk never splits a
/// codepoint, so an offset inside a character escapes it in the same chunk
/// whichever way it goes, and a chunk's own start is already a boundary.
fn clip_within(chunk: &str, chunk_start: usize, offset: usize, bias: Bias) -> usize {
    let local = offset - chunk_start;
    if chunk.is_char_boundary(local) {
        return offset;
    }

    let clipped = match bias {
        Bias::Left => {
            let mut c = local;
            while c > 0 && !chunk.is_char_boundary(c) {
                c -= 1;
            }
            c
        },
        Bias::Right => {
            let mut c = local;
            while c < chunk.len() && !chunk.is_char_boundary(c) {
                c += 1;
            }
            c
        },
    };
    chunk_start + clipped
}

fn ascii_pair_breaks(bytes: &[u8], local: usize) -> bool {
    if local == 0 || local >= bytes.len() {
        return false;
    }

    let (before, after) = (bytes[local - 1], bytes[local]);
    before < 0x80 && after < 0x80 && !(before == b'\r' && after == b'\n')
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
fn chunk_bitmaps(text: &str) -> ChunkBitmaps {
    const LANE: usize = 8;
    let mut char_lanes = [0u8; MAX_BASE / LANE];
    let mut wide_lanes = [0u8; MAX_BASE / LANE];
    let mut newline_lanes = [0u8; MAX_BASE / LANE];
    let mut tab_lanes = [0u8; MAX_BASE / LANE];
    let mut single_width_lanes = [0u8; MAX_BASE / LANE];

    for (lane_ix, lane) in text.as_bytes().chunks(LANE).enumerate() {
        let (mut chars, mut wide, mut newlines) = (0u8, 0u8, 0u8);
        let (mut tabs, mut single_width) = (0u8, 0u8);
        for (ix, &byte) in lane.iter().enumerate() {
            chars |= u8::from(byte & 0xC0 != 0x80) << ix;
            newlines |= u8::from(byte == b'\n') << ix;
            // A byte this large opens a four-byte sequence, which is the only
            // encoding costing two UTF-16 code units.
            wide |= u8::from(byte >= 240) << ix;
            tabs |= u8::from(byte == b'\t') << ix;
            // Printable ASCII, each character one byte and one cell. Control
            // bytes and anything multi-byte are left out, since their width is
            // either positional or needs the character to answer.
            single_width |= u8::from(byte.is_ascii_graphic() || byte == b' ') << ix;
        }
        char_lanes[lane_ix] = chars;
        wide_lanes[lane_ix] = wide;
        newline_lanes[lane_ix] = newlines;
        tab_lanes[lane_ix] = tabs;
        single_width_lanes[lane_ix] = single_width;
    }

    let chars = Bitmap::from_le_bytes(char_lanes);
    ChunkBitmaps {
        chars,
        chars_utf16: (Bitmap::from_le_bytes(wide_lanes) << 1) | chars,
        newlines: Bitmap::from_le_bytes(newline_lanes),
        tabs: Bitmap::from_le_bytes(tab_lanes),
        single_width: Bitmap::from_le_bytes(single_width_lanes),
    }
}

/// The per-byte maps a chunk keeps over its text.
struct ChunkBitmaps {
    chars: Bitmap,
    chars_utf16: Bitmap,
    newlines: Bitmap,
    tabs: Bitmap,
    single_width: Bitmap,
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
            // A middle row tied with the last. Both summarizers have to keep
            // the same one, or the same text summarizes differently depending
            // on how it happened to be chunked.
            "ab\ncccccc\ndddddd",
            "ab\ndddddd\ncccccc\ndddddd",
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

    /// A column past its row's end names the row's end, in every conversion
    /// that reads one.
    ///
    /// Row 0 here holds one byte and row 1 holds four, so a column of 3 on row
    /// 0 is out of range by two. Left uncapped it would count into row 1 and
    /// answer a position on a different row, which makes the UTF-8 and UTF-16
    /// conversions cease to be inverses of one another. Callers that carry a
    /// point from one rope to another rely on the two agreeing.
    #[test]
    fn a_column_past_the_row_end_lands_on_the_row_end() {
        let rope = Rope::from("a\nbbbb");

        assert_eq!(
            rope.clip_point(Point::new(0, 3), Bias::Right),
            Point::new(0, 1)
        );
        assert_eq!(rope.points_to_offsets_batch(&[Point::new(0, 3)]), vec![1]);
        assert_eq!(rope.point_to_offset(Point::new(0, 3)), 1);
        assert_eq!(
            rope.point_to_point_utf16(Point::new(0, 3)),
            PointUtf16::new(0, 1)
        );
        assert_eq!(rope.point_utf16_to_offset(PointUtf16::new(0, 3)), 1);
        assert_eq!(
            rope.point_utf16_to_point(PointUtf16::new(0, 3)),
            Point::new(0, 1)
        );
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

    /// Every other clip fixture fits in one chunk, where any chunk lookup lands
    /// on the right one. This one puts the character past a chunk boundary, so
    /// the lookup has to find the chunk holding the offset rather than the
    /// first.
    #[test]
    fn clip_offset_mid_char_in_a_later_chunk() {
        let (rope, at) = mid_char_in_a_later_chunk();
        assert_eq!(rope.clip_offset(at + 1, Bias::Left), at);
        assert_eq!(rope.clip_offset(at + 1, Bias::Right), at + 2);
        assert_eq!(rope.clip_offset(at + 3, Bias::Left), at + 2);
        assert_eq!(rope.clip_offset(at + 3, Bias::Right), at + 5);
        assert_eq!(rope.clip_offset(at, Bias::Left), at, "already a boundary");
    }

    /// The grapheme entry points clip an off-boundary offset before they step,
    /// and the cluster fixtures only ever hand them offsets already on one. This
    /// covers the clip they do, in a chunk that is not the first.
    #[test]
    fn grapheme_steps_from_mid_char_in_a_later_chunk() {
        let (rope, at) = mid_char_in_a_later_chunk();
        assert_eq!(
            rope.next_grapheme_boundary(at + 1),
            at + 2,
            "clipped back onto the character, then forward over it",
        );
        assert_eq!(
            rope.prev_grapheme_boundary(at + 1),
            at - 1,
            "clipped back onto the character, then back over the one before",
        );
        // Both biases land on `at`, since the char clip gets there first and it
        // is already a cluster boundary. The bias only decides which way to
        // escape a cluster, and this offset is inside a character rather than
        // inside a cluster of several.
        assert_eq!(rope.clip_to_grapheme_boundary(at + 1, Bias::Left), at);
        assert_eq!(rope.clip_to_grapheme_boundary(at + 1, Bias::Right), at);
    }

    /// A multi-chunk rope and the offset of a two-byte character living past the
    /// first chunk, so an offset one past it is inside a character and inside a
    /// later chunk at once.
    fn mid_char_in_a_later_chunk() -> (Rope, usize) {
        let head = "a".repeat(MAX_BASE + 5);
        let rope = Rope::from(format!("{head}h\u{00e9}\u{4e16}").as_str());
        assert!(rope.chunks().count() > 1, "the fixture has to span chunks");
        let at = head.len() + 1;
        (rope, at)
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

    /// A caller adds a run's byte length in place of measuring it, so the flag
    /// has to be false for anything whose width is not one cell per byte.
    /// Getting that wrong misplaces a cursor rather than failing.
    #[test]
    fn a_measured_chunk_promises_one_cell_per_byte() {
        let cases = [
            ("plain ascii", true),
            ("!@#$%^&*()", true),
            ("", true),
            ("with\ttab", false),
            ("wide \u{4e00}", false),
            ("mark e\u{301}", false),
            ("bell \u{7}", false),
            ("line\nbreak", false),
        ];

        for (text, expected) in cases {
            let rope = Rope::from(text);
            let measured: Vec<bool> = rope
                .measured_chunks_in_range(0..rope.len())
                .map(|chunk| chunk.cell_per_byte)
                .collect();

            assert_eq!(
                measured.iter().all(|&flag| flag),
                expected,
                "{text:?} was measured as {measured:?}"
            );
        }
    }

    /// The flag describes the slice a caller is handed, not the chunk it came
    /// out of, so a range landing inside a plain stretch of an otherwise
    /// awkward chunk still takes the cheap path.
    #[test]
    fn a_measured_chunk_describes_the_slice_not_its_chunk() {
        let rope = Rope::from("ab\tcd");

        let plain: Vec<&str> = rope
            .measured_chunks_in_range(3..5)
            .filter(|chunk| chunk.cell_per_byte)
            .map(|chunk| chunk.text)
            .collect();
        assert_eq!(plain, vec!["cd"], "the span past the tab is plain");

        let over_tab: Vec<bool> = rope
            .measured_chunks_in_range(0..5)
            .map(|chunk| chunk.cell_per_byte)
            .collect();
        assert_eq!(over_tab, vec![false], "the span containing it is not");
    }

    /// An inverted range covers no text, so every form of the walk yields
    /// nothing rather than slicing a chunk backwards.
    ///
    /// A range built from two offsets derived apart from each other can arrive
    /// out of order, and answering it with a panic makes that a crash in the
    /// caller rather than an empty span it can carry on from.
    #[test]
    fn an_inverted_range_covers_nothing() {
        let rope = Rope::from("hello world");
        let start = 5;
        let end = 3;

        assert_eq!(rope.chunks_in_range(start..end).count(), 0);
        assert_eq!(rope.bytes_in_range(start..end).count(), 0);
        assert_eq!(rope.reversed_chunks_in_range(start..end).count(), 0);
    }

    /// The same contract, for the two entry points that build a cursor and hand
    /// it the end. Both subtract that end from a chunk start, so an end below
    /// the cursor's own chunk underflows rather than answering.
    ///
    /// The rope spans several chunks so the end lands below the cursor's chunk
    /// rather than merely below the cursor inside it, which is the pair that
    /// underflows.
    #[test]
    fn an_inverted_range_slices_and_summarizes_to_nothing() {
        let rope = Rope::from("abcdefghij".repeat(400).as_str());
        assert!(
            rope.chunks().count() > 1,
            "the fixture has to span chunks for the end to fall below one",
        );
        let start = rope.len() - 1;
        let end = 2;

        assert_eq!(rope.slice(start..end).to_string(), "");
        assert_eq!(
            rope.text_summary_for_range(start..end),
            TextSummary::default()
        );
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

    /// A column naming the second half of a surrogate pair resolves to whichever
    /// end of that character the bias asks for.
    ///
    /// A UTF-16 column can land inside a character the rope cannot address
    /// there, and the bias is the caller saying which way to leave it. Left has
    /// to go left. An LSP position on a surrogate half is clipped Left, so
    /// answering with the character's end puts the caller a whole character
    /// past what they named.
    #[test]
    fn a_left_clip_of_a_surrogate_half_lands_on_the_character_start() {
        let rope = Rope::from("\u{1d11e}");
        assert_eq!(
            rope.clip_point_utf16(PointUtf16::new(0, 1), Bias::Left),
            PointUtf16::new(0, 0),
            "left of the pair's second unit is the character's start",
        );
        assert_eq!(
            rope.clip_point_utf16(PointUtf16::new(0, 1), Bias::Right),
            PointUtf16::new(0, 2),
            "right of it is the character's end",
        );
        assert_eq!(
            rope.clip_point_utf16(PointUtf16::new(0, 3), Bias::Right),
            PointUtf16::new(0, 2),
            "a column past the row clamps to its end either way",
        );
    }

    /// The same rule on a row the chunk does not start at, so the clip is
    /// measured from the row's own start rather than the chunk's.
    #[test]
    fn a_left_clip_of_a_surrogate_half_holds_on_a_later_row() {
        let rope = Rope::from("x\n\u{1d11e}\u{1d11e}\ny");
        for (column, left, right) in [(1u32, 0u32, 2u32), (3, 2, 4)] {
            assert_eq!(
                rope.clip_point_utf16(PointUtf16::new(1, column), Bias::Left),
                PointUtf16::new(1, left),
                "left clip of column {column}",
            );
            assert_eq!(
                rope.clip_point_utf16(PointUtf16::new(1, column), Bias::Right),
                PointUtf16::new(1, right),
                "right clip of column {column}",
            );
        }
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
    fn a_batch_answers_the_same_however_its_offsets_are_ordered() {
        // Ascending input skips the permutation and is visited as it stands, so
        // the two routes through the walk have to land on the same points. A
        // shuffled copy carries its own answer back into the original order.
        // Long enough to span many chunks, since a cursor left behind by an
        // out-of-order offset only lands somewhere visibly wrong once the
        // offsets are chunks apart.
        let text: String = (0..40).map(|i| format!("line{i} of the rope\n")).collect();
        let rope = Rope::from(text.as_str());

        let ascending: Vec<usize> = (0..12).map(|i| i * (rope.len() / 13)).collect();
        assert!(ascending.is_sorted(), "the sorted route is the one taken");

        let shuffled_order = [7usize, 0, 11, 3, 9, 1, 5, 10, 2, 8, 4, 6];
        let shuffled: Vec<usize> = shuffled_order.iter().map(|&i| ascending[i]).collect();
        assert!(!shuffled.is_sorted(), "and the other route for this one");

        let straight = rope.offsets_to_points_batch(&ascending);
        let permuted = rope.offsets_to_points_batch(&shuffled);

        let restored: Vec<Point> = {
            let mut back = vec![Point::zero(); ascending.len()];
            for (slot, &i) in shuffled_order.iter().enumerate() {
                back[i] = permuted[slot];
            }
            back
        };
        assert_eq!(straight, restored);
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

    /// A rope straddling several chunks, several rows, and several character
    /// widths, so a batch walking it forward meets every seam a scalar
    /// descending from the root would.
    fn seamed_rope() -> Rope {
        let text = format!(
            "{}\u{1F1F7}\u{1F1F8}\ne\u{301}{}\n\u{4e16}\u{754c}\n{}",
            "a".repeat(9),
            "b".repeat(10),
            "c".repeat(20),
        );
        let mut rope = Rope::new();
        for ch in text.chars() {
            rope.push(&ch.to_string());
        }
        assert!(rope.chunks().count() > 1, "the rope must straddle chunks");
        assert!(rope.max_point().row > 1, "and hold several rows");
        rope
    }

    /// Every offset answers what the scalar conversion answers, including the
    /// ones past the end and the ones splitting a character.
    ///
    /// The batch exists to walk the tree forward from one cursor rather than
    /// descend from the root per offset, so agreeing with the scalar is the
    /// whole of its contract. Reversing the input is what sends it down the
    /// permutation route instead of the ascending one, and repeating an offset
    /// is what asks the forward-only cursor to answer the same position twice.
    #[test]
    fn batched_point_conversions_match_the_scalar_ones() {
        let rope = seamed_rope();

        let offsets: Vec<usize> = (0..=rope.len() + 2).collect();
        let points: Vec<Point> = offsets.iter().map(|&o| rope.offset_to_point(o)).collect();

        assert_eq!(
            rope.offsets_to_points_batch(&offsets),
            points,
            "every batched point matches the scalar one",
        );
        assert_eq!(
            rope.offsets_to_points_batch(&reversed(&offsets)),
            reversed(&points),
            "descending offsets are permuted rather than mis-answered",
        );
        assert_eq!(
            rope.offsets_to_points_batch(&doubled(&offsets)),
            doubled(&points),
            "a repeated offset is answered twice rather than skipped",
        );
    }

    /// The mirror of [`batched_point_conversions_match_the_scalar_ones`], over
    /// every point rather than every offset.
    ///
    /// The columns run two past each row's length and the rows one past the
    /// last, so the clamping the scalar applies to a position outside the text
    /// has to survive the batch's forward walk too.
    #[test]
    fn batched_offset_conversions_match_the_scalar_ones() {
        let rope = seamed_rope();

        let points: Vec<Point> = (0..=rope.max_point().row + 1)
            .flat_map(|row| (0..=rope.line_len(row) + 2).map(move |column| Point::new(row, column)))
            .collect();
        let offsets: Vec<usize> = points.iter().map(|&p| rope.point_to_offset(p)).collect();

        assert_eq!(
            rope.points_to_offsets_batch(&points),
            offsets,
            "every batched offset matches the scalar one",
        );
        assert_eq!(
            rope.points_to_offsets_batch(&reversed(&points)),
            reversed(&offsets),
            "descending points are permuted rather than mis-answered",
        );
        assert_eq!(
            rope.points_to_offsets_batch(&doubled(&points)),
            doubled(&offsets),
            "a repeated point is answered twice rather than skipped",
        );
    }

    fn reversed<T: Copy>(xs: &[T]) -> Vec<T> {
        xs.iter().rev().copied().collect()
    }

    fn doubled<T: Copy>(xs: &[T]) -> Vec<T> {
        xs.iter().flat_map(|&x| [x, x]).collect()
    }

    /// The batch walks the tree forward once where the scalar calls each
    /// descend from the root, so the only thing that makes it worth having is
    /// that every offset comes back with the answer the scalar gives.
    #[test]
    fn batched_rope_reads_match_the_scalar_ones() {
        let text = format!(
            "{}\u{1F1F7}\u{1F1F8}e\u{301}{}",
            "a".repeat(9),
            "b".repeat(24),
        );
        let mut rope = Rope::new();
        for ch in text.chars() {
            rope.push(&ch.to_string());
        }
        assert!(rope.chunks().count() > 1, "the rope must straddle chunks");

        // Every offset, including the ones splitting a scalar, which the batch
        // cannot settle from the chunk in hand and hands to the scalar path.
        let steps: Vec<usize> = (0..=rope.len()).collect();
        let expected_steps: Vec<usize> = steps
            .iter()
            .map(|&o| rope.prev_grapheme_boundary(o))
            .collect();
        // The char read is only pinned against the scalar on char boundaries.
        // Off one, the batch answers None while chars_at slices from the split
        // offset and panics, so this filter is what keeps the comparison to
        // where the two are meant to agree.
        let reads: Vec<usize> = steps
            .iter()
            .copied()
            .filter(|&o| rope.is_char_boundary(o))
            .collect();
        let expected_reads: Vec<Option<char>> =
            reads.iter().map(|&o| rope.chars_at(o).next()).collect();

        let expected_forward: Vec<usize> = steps
            .iter()
            .map(|&o| rope.next_grapheme_boundary(o))
            .collect();
        // Both biases at every offset, so the two escape sub-passes and the
        // walk that feeds them are all exercised over the same input.
        let clips: Vec<(usize, Bias)> = steps
            .iter()
            .flat_map(|&o| [(o, Bias::Left), (o, Bias::Right)])
            .collect();
        let expected_clips: Vec<usize> = clips
            .iter()
            .map(|&(o, bias)| rope.clip_to_grapheme_boundary(o, bias))
            .collect();

        assert_eq!(
            rope.prev_grapheme_boundaries_batch(&steps),
            expected_steps,
            "every batched cluster step matches the scalar one",
        );
        assert_eq!(
            rope.next_grapheme_boundaries_batch(&steps),
            expected_forward,
            "every batched forward step matches the scalar one",
        );
        assert_eq!(
            rope.clip_to_grapheme_boundaries_batch(&clips),
            expected_clips,
            "every batched clip matches the scalar one, either bias",
        );
        assert_eq!(
            rope.chars_at_batch(&reads),
            expected_reads,
            "every batched character read matches the scalar one",
        );

        let reverse = |xs: &[usize]| -> Vec<usize> { xs.iter().rev().copied().collect() };
        assert_eq!(
            rope.prev_grapheme_boundaries_batch(&reverse(&steps)),
            reverse(&expected_steps),
            "descending input is permuted rather than mis-answered",
        );
        assert_eq!(
            rope.next_grapheme_boundaries_batch(&reverse(&steps)),
            reverse(&expected_forward),
            "descending input is permuted rather than mis-answered",
        );
        assert_eq!(
            rope.clip_to_grapheme_boundaries_batch(
                &clips.iter().rev().copied().collect::<Vec<_>>()
            ),
            reverse(&expected_clips),
            "descending input is permuted rather than mis-answered",
        );
        assert_eq!(
            rope.chars_at_batch(&reverse(&reads)),
            expected_reads.iter().rev().copied().collect::<Vec<_>>(),
            "descending input is permuted rather than mis-answered",
        );
    }

    /// The stepping pair answers most offsets from two bytes of the chunk in
    /// hand, so what it must agree with is not its own slower path but the
    /// segmentation rules themselves, read off the whole text at once.
    #[test]
    fn stepping_matches_the_segmentation_rules_at_every_offset() {
        use unicode_segmentation::UnicodeSegmentation;

        for pad in 0..24usize {
            // ASCII either side of a flag pair, a ZWJ family, a CRLF, a
            // combining mark and an Arabic number sign, so the fast path meets
            // each of the things that defeat it. The number sign is the one
            // that reaches forward. GB9b joins a Prepend to whatever follows,
            // so the ASCII digit after it starts no cluster of its own, and
            // that is why the scalar before an ASCII one has to be checked as
            // well as the ASCII one. The pad walks all of them across the
            // chunk seams.
            let text = format!(
                "{}ab\r\ncd\u{1F1F7}\u{1F1F8}e\u{301}f\u{1F468}\u{200D}\u{1F469}g\u{0600}7h\r\nij",
                "z".repeat(pad),
            );
            let mut rope = Rope::new();
            for ch in text.chars() {
                rope.push(&ch.to_string());
            }

            let mut boundaries: Vec<usize> =
                text.grapheme_indices(true).map(|(at, _)| at).collect();
            boundaries.push(text.len());

            for offset in (0..=text.len()).filter(|&o| text.is_char_boundary(o)) {
                let next = boundaries.iter().copied().find(|&b| b > offset);
                assert_eq!(
                    rope.next_grapheme_boundary(offset),
                    next.unwrap_or(offset),
                    "next from {offset} at pad {pad} in {text:?}",
                );

                let prev = boundaries.iter().copied().rev().find(|&b| b < offset);
                assert_eq!(
                    rope.prev_grapheme_boundary(offset),
                    prev.unwrap_or(0),
                    "prev from {offset} at pad {pad} in {text:?}",
                );
            }
        }
    }

    #[test]
    fn clipping_to_a_boundary_leaves_one_alone() {
        // The distinction from the stepping pair. Every one of these offsets is
        // already a boundary, so a clamp must not move any of them, where a
        // step would move all of them.
        let rope = Rope::from("ae\u{301}b");
        for offset in [0, 1, 4, 5] {
            for bias in [Bias::Left, Bias::Right] {
                assert_eq!(
                    rope.clip_to_grapheme_boundary(offset, bias),
                    offset,
                    "offset {offset} is a boundary already, under {bias:?}",
                );
            }
        }
    }

    #[test]
    fn clipping_escapes_a_cluster_in_the_asked_direction() {
        // Byte 2 sits between the e and its combining acute.
        let rope = Rope::from("ae\u{301}b");
        assert_eq!(rope.clip_to_grapheme_boundary(2, Bias::Left), 1);
        assert_eq!(rope.clip_to_grapheme_boundary(2, Bias::Right), 4);
    }

    #[test]
    fn clipping_holds_at_the_rope_ends() {
        let rope = Rope::from("e\u{301}");
        for bias in [Bias::Left, Bias::Right] {
            assert_eq!(rope.clip_to_grapheme_boundary(0, bias), 0);
            assert_eq!(rope.clip_to_grapheme_boundary(rope.len(), bias), rope.len());
            assert_eq!(Rope::new().clip_to_grapheme_boundary(0, bias), 0);
        }
    }

    #[test]
    fn clipping_holds_a_crlf_pair_together() {
        // The one ASCII pair that does not break, so the ASCII shortcut has to
        // decline it and let the cursor answer.
        let rope = Rope::from("a\r\nb");
        assert_eq!(rope.clip_to_grapheme_boundary(2, Bias::Left), 1);
        assert_eq!(rope.clip_to_grapheme_boundary(2, Bias::Right), 3);

        for offset in [1, 3] {
            for bias in [Bias::Left, Bias::Right] {
                assert_eq!(
                    rope.clip_to_grapheme_boundary(offset, bias),
                    offset,
                    "offset {offset} brackets the pair rather than splitting it, under {bias:?}",
                );
            }
        }
    }

    /// Byte offsets where one chunk of `rope` ends and the next begins.
    fn chunk_seams(rope: &Rope) -> Vec<usize> {
        let mut at = 0;
        rope.chunks()
            .map(|chunk| {
                at += chunk.len();
                at
            })
            .filter(|&seam| seam < rope.len())
            .collect()
    }

    #[test]
    fn clipping_answers_an_offset_on_a_chunk_seam() {
        // Deciding a boundary from one chunk means the two neighbouring bytes
        // have to be in it, which at a seam they are not. Chunks cap at 16 bytes
        // under cfg(test), so a plain run of letters is enough to make seams.
        let rope = grown_rope(&"a".repeat(40));
        let seams = chunk_seams(&rope);
        assert!(!seams.is_empty(), "the fixture must span chunks");

        for seam in seams {
            for bias in [Bias::Left, Bias::Right] {
                assert_eq!(
                    rope.clip_to_grapheme_boundary(seam, bias),
                    seam,
                    "seam {seam} falls between two ASCII letters, under {bias:?}",
                );
            }
        }
    }

    #[test]
    fn clipping_escapes_a_cluster_a_seam_runs_through() {
        // A seam can land between a base character and its combining mark, where
        // the byte before the offset is in the previous chunk and says nothing
        // about the cluster. Deciding from the chunk holding the offset alone
        // would call this a boundary, and it is the middle of one.
        let cluster_split: Vec<usize> = (1..40)
            .filter(|pad| {
                let rope = grown_rope(&format!("{}e\u{301}{}", "a".repeat(*pad), "b".repeat(24)));
                chunk_seams(&rope).contains(&(pad + 1))
            })
            .collect();
        assert!(
            !cluster_split.is_empty(),
            "no padding put a seam inside the cluster, so this test proves nothing",
        );

        for pad in cluster_split {
            let rope = grown_rope(&format!("{}e\u{301}{}", "a".repeat(pad), "b".repeat(24)));
            assert_eq!(
                rope.clip_to_grapheme_boundary(pad + 1, Bias::Left),
                pad,
                "clamping back off the acute across the seam at pad {pad}",
            );
            assert_eq!(
                rope.clip_to_grapheme_boundary(pad + 1, Bias::Right),
                pad + 3,
                "clamping forward off the acute across the seam at pad {pad}",
            );
        }
    }

    /// A rope built one character at a time, so chunks split where pushing put
    /// them rather than all at once over the whole string.
    fn grown_rope(text: &str) -> Rope {
        let mut rope = Rope::new();
        for ch in text.chars() {
            rope.push(&ch.to_string());
        }
        rope
    }

    #[test]
    fn clipping_reaches_across_a_chunk_boundary() {
        // The cursor has to be handed the text before the offset to answer, and
        // that text can live in an earlier chunk. Chunks cap at 16 bytes under
        // cfg(test), so the padding only has to clear that.
        for pad in 16..24usize {
            let mut rope = Rope::new();
            for ch in format!("{}e\u{301}{}", "a".repeat(pad), "b".repeat(24)).chars() {
                rope.push(&ch.to_string());
            }
            assert!(rope.chunks().count() > 1, "pad {pad} must span chunks");

            let inside = pad + 1;
            assert_eq!(
                rope.clip_to_grapheme_boundary(inside, Bias::Left),
                pad,
                "clamping back off the acute at pad {pad}",
            );
            assert_eq!(
                rope.clip_to_grapheme_boundary(inside, Bias::Right),
                pad + 3,
                "clamping forward off the acute at pad {pad}",
            );
            assert_eq!(
                rope.clip_to_grapheme_boundary(pad, Bias::Right),
                pad,
                "the cluster start is a boundary at pad {pad}",
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
        let mut column = 0u32;
        let mut bytes = 0u32;
        for ch in text[row_start(text, point.row)..].chars() {
            if ch == '\n' || bytes >= point.column {
                break;
            }
            bytes += ch.len_utf8() as u32;
            column += ch.len_utf16() as u32;
        }
        PointUtf16::new(point.row, column)
    }

    /// Byte offset of `target.column` bytes into `target.row`, stopping at the
    /// end of the line.
    fn point_to_offset(text: &str, target: Point) -> usize {
        let start = row_start(text, target.row);
        let line_len = text[start..].split('\n').next().map_or(0, str::len);
        start + (target.column as usize).min(line_len)
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
                    assert_eq!(
                        rope.point_to_offset(point),
                        point_to_offset(&text, point),
                        "point_to_offset at {point:?} of {text:?}"
                    );
                    // A column landing inside a character has no UTF-16 answer
                    // the two agree on, since a surrogate pair is one bit in
                    // the chunk bitmap and two units in a char walk. A column
                    // past the row's end clamps onto it, which is a boundary.
                    let clamped = row_start(&text, row) + (column as usize).min(line_bytes);
                    if text.is_char_boundary(clamped) {
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
    ///
    /// A column can name the second unit of a surrogate pair, which is not a
    /// position the rope can address. The bias decides which end of that
    /// character to answer with, so this walk has to honour it rather than
    /// always running the character to completion.
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
            let char_start = bytes;
            utf16 += ch.len_utf16() as u32;
            bytes += ch.len_utf8();
            if utf16 > point.column {
                if matches!(bias, Bias::Left) {
                    bytes = char_start;
                }
                break;
            }
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

    /// A walk whose first row is already past the rope's last one yields
    /// nothing.
    ///
    /// Rows past the end are absent, not empty. The walk is documented to
    /// return fewer items than asked for rather than empty ones, and a caller
    /// rendering what it yields would otherwise paint a row the rope does not
    /// have.
    #[test]
    fn a_walk_starting_past_the_last_row_yields_nothing() {
        let rope = Rope::from("abc\ndef");
        assert_eq!(rope.max_point().row, 1, "two rows, indices zero and one");

        let mut walk = rope.line_walk(5..8);
        assert_eq!(walk.next_len(), None, "row five is past the last row");

        let mut walk = rope.line_walk(2..4);
        assert_eq!(walk.next_len(), None, "so is the row just past the end");

        // The last real row still reports, including its length, so the guard
        // stops one row too late rather than one too early.
        let mut walk = rope.line_walk(1..4);
        assert_eq!(walk.next_len(), Some((1, 3)), "row one is the last one");
        assert_eq!(walk.next_len(), None, "and nothing follows it");
    }

    /// A trailing newline makes the empty row after it real, so a walk reaches
    /// it.
    ///
    /// The row past the end and the empty final row are one apart and easy to
    /// conflate, and `max_point` is what separates them.
    #[test]
    fn a_walk_reaches_the_empty_row_after_a_trailing_newline() {
        let rope = Rope::from("abc\ndef\n");
        assert_eq!(rope.max_point().row, 2, "the empty row after the newline");

        let mut walk = rope.line_walk(2..5);
        assert_eq!(walk.next_len(), Some((2, 0)), "the empty row is real");
        assert_eq!(walk.next_len(), None, "and it is the last one");
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

    /// Typing runs the same seam over and over. Each keystroke rebuilds the
    /// rope around one offset, so a chunk left short there is left short again
    /// by the next one. Scattered edits spread the damage and hide this.
    #[test]
    fn typing_at_one_spot_leaves_the_chunk_count_near_the_text() {
        let mut rope = Rope::from("abcdefghij".repeat(20).as_str());
        for i in 0..40 {
            let at = 100 + i;
            rope.replace(at..at, "x");
        }
        rope.assert_chunks_dense();

        let floor = rope.len().div_ceil(MAX_BASE);
        assert!(
            chunk_count(&rope) <= floor * 3 / 2,
            "after 40 inserts at one spot over {} bytes the rope holds {} chunks, against {floor} for the text",
            rope.len(),
            chunk_count(&rope),
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
        rope.assert_chunks_dense();

        let floor = rope.len().div_ceil(MAX_BASE);
        assert!(
            chunk_count(&rope) <= floor * 3 / 2,
            "after 300 edits over {} bytes the rope holds {} chunks, against {floor} for the text",
            rope.len(),
            chunk_count(&rope),
        );
    }
}

/// Whether a rope still holds the text its edits describe.
///
/// The chunk-density tests above drive random edits but read only the shape of
/// the tree afterwards, so a rope could return the wrong text under some edit
/// sequence and every one of them would still pass. These compare against a
/// reference the edits are applied to in parallel.
///
/// The summary is checked beside the content because callers treat an unequal
/// summary as proof of unequal text without reading either, so a summary that
/// drifts from the text it describes is as wrong as the text being wrong.
#[cfg(test)]
mod edit_oracle_tests {
    use super::*;

    /// Multi-byte throughout, since the summary counts UTF-16 lengths,
    /// characters and per-row characters that ASCII alone would never separate.
    /// The last entry is empty, which makes a replace a pure deletion.
    const ALPHABET: [&str; 8] = [
        "a",
        "z\n",
        "\n",
        "e\u{301}",
        "\u{4e16}\u{754c}",
        "\u{1d11e}",
        "\u{1f389}",
        "",
    ];

    #[test]
    fn random_replaces_keep_the_text_and_summary_the_edits_describe() {
        let mut seed = 0x5dee_ce66_d123_4567_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let base = "abc\n\u{4e16}\u{754c}\u{e9}\n\u{1d11e}xy\nhello\n".repeat(12);
        let mut rope = Rope::from(base.as_str());
        let mut reference = base;

        for step in 0..300 {
            let len = rope.len();

            // Clipping in the rope is enough for both, since the reference
            // holds the same bytes, and `replace` needs character boundaries.
            let at = rope.clip_offset((next() as usize) % (len + 1), Bias::Left);
            let end = rope.clip_offset((at + (next() as usize) % 12).min(len), Bias::Right);
            let insert = ALPHABET[(next() as usize) % ALPHABET.len()];

            rope.replace(at..end, insert);
            reference.replace_range(at..end, insert);

            assert_eq!(
                rope.to_string(),
                reference,
                "step {step}: replacing {at}..{end} with {insert:?}",
            );
            assert_eq!(
                rope.summary(),
                &TextSummary::from_str(&reference),
                "step {step}: summary after replacing {at}..{end} with {insert:?}",
            );
        }
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
            // A tie between a middle row and the last, with a shorter row
            // first. The two summarizers walk the rows in different orders,
            // and only this shape puts them on different answers.
            "ab\ncccccc\ndddddd",
            "ab\ndddddd\ncccccc\ndddddd",
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

            // Against the text's own summary rather than each other, so a
            // composition that is consistently wrong across every layout is
            // caught too.
            let from_str = TextSummary::from_str(text);
            for (label, rope) in &built {
                assert_eq!(
                    rope.summary(),
                    &from_str,
                    "{label} summarises {text:?} differently from the text itself"
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
    use std::ops::Range;

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

    /// A range starting inside a word, inside a line, and inside a chunk, which
    /// is where a slice and a span disagree and where the clipping has to be
    /// right.
    fn mid_word_range(text: &str) -> Range<usize> {
        let from = text.find("needle").expect("the fixture has one") + 3;
        let to = text.rfind("gamma").expect("the fixture has one") + 3;
        from..to
    }

    #[test]
    fn a_slice_matches_as_though_it_were_the_whole_text() {
        let text = straddling();
        let rope = Rope::from(text.as_str());
        let range = mid_word_range(&text);
        let piece = &text[range.clone()];

        for pattern in [
            "needle",
            "^caf",
            "^ +needle",
            r"\bneedle\b",
            r"\Adle",
            "tail$",
            "row [0-9]+",
        ] {
            let regex = Regex::new(pattern).expect("the pattern compiles");
            assert_eq!(
                spans(&regex, rope.regex_slice_input(range.clone())),
                spans(&regex, Input::new(piece)),
                "slice and standalone differ for {pattern:?}"
            );
        }
    }

    /// A slice beginning exactly on a chunk boundary is the case that makes the
    /// engine step back a chunk to look behind, rather than reading the byte
    /// before it within the chunk it is already on. Stepping back has to stop at
    /// the slice, not walk into the text in front of it.
    #[test]
    fn a_slice_starting_on_a_chunk_boundary_does_not_look_behind_it() {
        let text = straddling();
        let rope = Rope::from(text.as_str());

        let boundary = rope.chunks().next().expect("a first chunk").len();
        assert!(
            text.as_bytes()[boundary - 1].is_ascii_alphanumeric()
                && text.as_bytes()[boundary].is_ascii_alphanumeric(),
            "the boundary has to fall mid-word for a word boundary to be in question"
        );

        let range = boundary..text.len();
        let piece = &text[range.clone()];

        for pattern in [r"\A\w", r"\b\w", "^."] {
            let regex = Regex::new(pattern).expect("the pattern compiles");
            assert_eq!(
                spans(&regex, rope.regex_slice_input(range.clone())),
                spans(&regex, Input::new(piece)),
                "slice and standalone differ for {pattern:?} at a chunk boundary"
            );
        }
    }

    /// The two inputs exist because they answer differently, so pin that they
    /// do. A slice starts where it was cut. A span is a position in text that
    /// carries on either side of it.
    #[test]
    fn a_slice_and_a_span_disagree_about_where_the_text_begins() {
        let text = straddling();
        let rope = Rope::from(text.as_str());
        let range = mid_word_range(&text);
        let regex = Regex::new(r"\Adle").expect("the pattern compiles");

        assert_eq!(
            spans(&regex, rope.regex_slice_input(range.clone())),
            vec![(0, 3)],
            "the slice starts where it was cut"
        );
        assert_eq!(
            spans(&regex, rope.regex_input(range)),
            Vec::new(),
            "the span is part-way through a text that started elsewhere"
        );
    }

    /// A span starts part-way through a text that carries on either side of it,
    /// so it reads what precedes its start and reports rope offsets. Pinned
    /// across a chunk boundary, where reading behind means stepping back a
    /// chunk, and where the haystack it is handed decides how far the engine
    /// walks to get going.
    #[test]
    fn a_span_from_mid_rope_looks_behind_its_start() {
        let text = straddling();
        let rope = Rope::from(text.as_str());
        let boundary = rope.chunks().next().expect("a first chunk").len();

        // The first line start past the first chunk boundary, so the newline
        // that anchors a match at the range's start is in an earlier chunk.
        let from = boundary + text[boundary..].find("\ncaf").expect("the fixture has one") + 1;

        let regex = Regex::new("(?m)^caf").expect("the pattern compiles");
        let found = spans(&regex, rope.regex_input(from..rope.len()));
        let expected: Vec<(usize, usize)> = spans(&regex, Input::new(text.as_str()))
            .into_iter()
            .filter(|&(start, _)| start >= from)
            .collect();

        assert_eq!(
            found.first().copied(),
            Some((from, from + 3)),
            "the newline before the range anchors a match at its very start"
        );
        assert_eq!(found, expected, "and the rest agree with the whole text");

        // The haystack the engine is handed sits on the chunk holding the
        // range's start, not on the rope's first, which is what it walks from.
        let holding = rope
            .chunks()
            .scan(0, |at, chunk| {
                let start = *at;
                *at += chunk.len();
                Some((start, chunk))
            })
            .find(|&(start, chunk)| (start..start + chunk.len()).contains(&from))
            .expect("some chunk holds it")
            .1;
        assert_eq!(
            rope.regex_input(from..rope.len()).chunk(),
            holding.as_bytes(),
            "the search starts on the chunk it is searching from",
        );
    }

    #[test]
    fn an_empty_rope_has_one_empty_chunk_and_no_matches() {
        let rope = Rope::new();
        let regex = Regex::new("needle").expect("the pattern compiles");

        assert_eq!(spans(&regex, rope.regex_input(0..0)), Vec::new());
    }

    #[test]
    fn a_slice_cursor_stops_at_the_slice_rather_than_the_rope() {
        use regex_cursor::Cursor;

        let text = straddling();
        let rope = Rope::from(text.as_str());
        let boundary = rope.chunks().next().expect("a first chunk").len();

        let mut cursor = rope.regex_cursor_over(boundary..text.len());
        assert_eq!(cursor.offset(), 0, "the slice starts where it was cut");
        assert!(
            !cursor.backtrack(),
            "nothing precedes a slice, so a step back reports none rather than \
             leaving an empty chunk behind"
        );
        assert!(
            !cursor.chunk().is_empty(),
            "and the chunk it is on still holds text"
        );

        while cursor.advance() {}
        assert_eq!(
            cursor.offset() + cursor.chunk().len(),
            text.len() - boundary,
            "the last chunk ends where the slice does"
        );
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
