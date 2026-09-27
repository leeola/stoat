use crate::{
    sum_tree::{self, ContextLessSummary, Dimension},
    Bias, Dimensions, Item, OffsetUtf16, Point, PointUtf16, SumTree,
};
use arrayvec::ArrayString;
use regex_cursor::{Cursor as RegexCursor, Input};
use std::{cmp, ops::Range};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::UnicodeWidthChar;

/// Display cells `ch` occupies on a terminal grid.
///
/// Two for a wide character, zero for a combining mark and for anything with
/// no width of its own, one otherwise. A tab is not positional here: it reports
/// its own width, and a caller that expands tabs to stops answers for it.
pub fn display_width(ch: char) -> u32 {
    ch.width().unwrap_or(0) as u32
}

/// Cells `ch` contributes to a summed width, a tab counting one.
///
/// A tab is one cell past a caller's tab-expansion cap, which is where a summed
/// count is read. [`TextSummary::cells`] states the contract.
fn cell_width(ch: char) -> u32 {
    match ch {
        '\t' => 1,
        _ => display_width(ch),
    }
}

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
    /// Display cells the text occupies, a tab counting one and a newline none.
    ///
    /// A tab's width is positional, so this is the width a tab has past a
    /// caller's tab-expansion cap, where every tab is one cell. Below the cap a
    /// caller expands tabs itself, and this does not answer for them.
    pub cells: u32,
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

        let mut cells = 0u32;

        for ch in text.chars() {
            len_utf16.0 += ch.len_utf16();
            chars += 1;
            cells += cell_width(ch);

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
            cells,
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
        self.cells += other.cells;
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

    /// Display cells this chunk's text occupies, per [`TextSummary::cells`].
    ///
    /// A byte marked `single_width` is one cell on its own, a tab is one, and a
    /// newline is none, so those three between them answer for most text
    /// outright. What they leave is every character start none of them
    /// decides: a multi-byte lead byte, or an ASCII control byte, which is
    /// worth no cells but has to be asked rather than assumed.
    ///
    /// Three cases, by how much of the chunk that mask holds:
    ///
    /// - Nothing: two popcounts, which is a chunk of plain ASCII text. That case is tested for
    ///   before the mask is built, so it pays nothing for the other two.
    /// - A quarter of the bytes or more: one walk of the characters. Every character needs its own
    ///   width there, and a walk beats a seek per character. Text in a wide script lands here.
    /// - Anything between: one seek per undecided start. Source text carrying an occasional
    ///   accented letter is this case, and it would otherwise pay a whole-chunk decode for one
    ///   character.
    fn cells(&self) -> u32 {
        let covered = self.single_width | self.tabs | self.newlines;
        if covered == below(self.text.len()) {
            return self.single_width.count_ones() + self.tabs.count_ones();
        }

        // Derived only past the test above, so a chunk of plain ASCII pays
        // nothing for it. A continuation byte starts no character and is in
        // none of the three, so it never reaches the mask.
        let undecided = self.chars & !covered;
        if undecided.count_ones() as usize * 4 >= self.text.len() {
            return self.text.chars().map(cell_width).sum();
        }

        let mut cells = self.single_width.count_ones() + self.tabs.count_ones();
        let mut bits = undecided;
        while bits != 0 {
            let at = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(ch) = self.text.as_str()[at..].chars().next() {
                cells += cell_width(ch);
            }
        }
        cells
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
            cells: self.cells(),
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

    /// Display cells the text before `offset` occupies, per
    /// [`TextSummary::cells`].
    ///
    /// One tree seek plus a walk of the chunk `offset` falls in, so the cost is
    /// the tree's depth rather than the text's length. `offset` is clamped to
    /// the rope's length.
    ///
    /// An offset inside a character counts that character's full width, which
    /// is what a column walk stopping at the same byte does.
    pub fn offset_to_cells(&self, offset: usize) -> u32 {
        let offset = offset.min(self.len());
        let (start, _end, chunk) =
            self.chunks
                .find::<Dimensions<usize, Cells>, _>((), &offset, Bias::Right);
        let Dimensions(chunk_start, cells_before, ()) = start;

        let Some(chunk) = chunk else {
            return cells_before.0;
        };
        let local = offset - chunk_start;
        let mut cells = cells_before.0;
        let mut seen = 0usize;
        for ch in chunk.text.as_str().chars() {
            if seen >= local {
                break;
            }
            cells += cell_width(ch);
            seen += ch.len_utf8();
        }
        cells
    }

    /// The offset `cells` display cells into the text, per
    /// [`TextSummary::cells`].
    ///
    /// A seek of the same cost as [`Self::offset_to_cells`], which it inverts
    /// where an inverse exists. A count landing inside a wide character answers
    /// that character's start under [`Bias::Left`] and its end under
    /// [`Bias::Right`]. A count past the text answers the rope's length.
    ///
    /// A character of no width shares its predecessor's cell count, so several
    /// offsets can answer one count and this returns the first of them. Round
    /// tripping through [`Self::offset_to_cells`] therefore holds in cells but
    /// not in offsets.
    pub fn cells_to_offset(&self, cells: u32, bias: Bias) -> usize {
        let (start, _end, chunk) =
            self.chunks
                .find::<Dimensions<Cells, usize>, _>((), &Cells(cells), Bias::Right);
        let Dimensions(cells_before, chunk_start, ()) = start;

        let Some(chunk) = chunk else {
            return self.len();
        };
        let mut seen = cells_before.0;
        for (local, ch) in chunk.text.as_str().char_indices() {
            if seen >= cells {
                return chunk_start + local;
            }
            let next = seen + cell_width(ch);
            if next > cells {
                // The count names a column inside this character.
                return match bias {
                    Bias::Left => chunk_start + local,
                    Bias::Right => chunk_start + local + ch.len_utf8(),
                };
            }
            seen = next;
        }
        chunk_start + chunk.text.len()
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

/// Prints the content as a quoted string, so a rope reads in a derived `Debug`
/// exactly as the `String` it stands in for did.
///
/// The whole content is materialized to do it, which is what any `Debug` of a
/// rope costs. Nothing on a hot path formats one.
impl std::fmt::Debug for Rope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.to_string(), f)
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
            {
                let chunk = self.chunks.item()?;
                self.local_offset = chunk.text.len()
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

/// A rope position counted in display cells, per [`TextSummary::cells`].
///
/// Seeking by this is what lets a caller measure a span's width without
/// walking it. See [`Rope::offset_to_cells`] and [`Rope::cells_to_offset`].
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cells(pub u32);

impl<'a> Dimension<'a, TextSummary> for Cells {
    fn zero(_cx: ()) -> Self {
        Cells(0)
    }

    fn add_summary(&mut self, summary: &'a TextSummary, _cx: ()) {
        self.0 += summary.cells;
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
mod tests;
