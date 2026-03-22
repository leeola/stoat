use crate::{
    sum_tree::{self, ContextLessSummary, Dimension},
    Bias, Dimensions, Item, OffsetUtf16, Point, PointUtf16, SumTree,
};
use arrayvec::ArrayString;
use std::{cmp, ops::Range};

#[cfg(not(test))]
type Bitmap = u128;
#[cfg(test)]
type Bitmap = u16;

const MAX_BASE: usize = Bitmap::BITS as usize;

#[derive(Clone, Default, Debug)]
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
    newlines: Bitmap,
    utf16_len: usize,
    text: ArrayString<MAX_BASE>,
}

impl Chunk {
    fn new(text: &str) -> Self {
        let mut chars: Bitmap = 0;
        let mut newlines: Bitmap = 0;
        let mut utf16_len = 0usize;

        for (i, &byte) in text.as_bytes().iter().enumerate() {
            let bit: Bitmap = 1 << i;
            if byte & 0xC0 != 0x80 {
                chars |= bit;
            }
            if byte == b'\n' {
                newlines |= bit;
            }
        }
        for ch in text.chars() {
            utf16_len += ch.len_utf16();
        }

        let mut arr = ArrayString::new();
        arr.push_str(text);

        Self {
            chars,
            newlines,
            utf16_len,
            text: arr,
        }
    }

    fn push_str(&mut self, s: &str) {
        let offset = self.text.len();
        self.text.push_str(s);
        for (i, &byte) in s.as_bytes().iter().enumerate() {
            let bit: Bitmap = 1 << (offset + i);
            if byte & 0xC0 != 0x80 {
                self.chars |= bit;
            }
            if byte == b'\n' {
                self.newlines |= bit;
            }
        }
        for ch in s.chars() {
            self.utf16_len += ch.len_utf16();
        }
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
            PointUtf16::new(0, self.utf16_len as u32)
        } else {
            let last_nl_byte = (Bitmap::BITS - 1 - self.newlines.leading_zeros()) as usize;
            let after_last_nl = &self.text.as_str()[last_nl_byte + 1..];
            let utf16_col: u32 = after_last_nl.chars().map(|ch| ch.len_utf16() as u32).sum();
            PointUtf16::new(row, utf16_col)
        };

        TextSummary {
            len: text_len,
            len_utf16: OffsetUtf16(self.utf16_len),
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

    pub fn append(&mut self, other: Rope) {
        self.chunks.append(other.chunks, ());
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
        indexed.sort_unstable_by(|a, b| a.1.cmp(&b.1));

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

    fn row_byte_range(&self, row: u32) -> Range<usize> {
        let max = self.max_point();
        if row > max.row {
            let len = self.len();
            return len..len;
        }
        let start = self.point_to_offset(Point::new(row, 0));
        if row >= max.row {
            return start..self.len();
        }
        let mut cursor = self.chunks.cursor::<usize>(());
        cursor.seek(&start, Bias::Right);
        let mut pos = start;
        while let Some(chunk) = cursor.item() {
            let chunk_start = *cursor.start();
            let local = pos - chunk_start;
            let nl_mask = chunk.newlines >> local;
            if nl_mask != 0 {
                let nl_offset = nl_mask.trailing_zeros() as usize;
                return start..(pos + nl_offset);
            }
            pos = chunk_start + chunk.text.len();
            cursor.next();
        }
        start..self.len()
    }

    pub fn line_len(&self, row: u32) -> u32 {
        let max = self.max_point();
        if row > max.row {
            return 0;
        }
        if row == max.row {
            return max.column;
        }
        let range = self.row_byte_range(row);
        (range.end - range.start) as u32
    }

    pub fn line_lens_in_range(&self, rows: Range<u32>) -> Vec<u32> {
        if rows.is_empty() {
            return Vec::new();
        }
        let max = self.max_point();
        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            if row > max.row {
                results.push(0);
            } else if row == max.row {
                results.push(max.column);
            } else {
                results.push(self.line_len(row));
            }
        }
        results
    }

    pub fn chunks_in_line(&self, row: u32) -> ChunksInRange<'_> {
        let range = self.row_byte_range(row);
        self.chunks_in_range(range)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        let max = self.max_point();
        if point.row > max.row {
            return max;
        }
        let len = self.line_len(point.row);
        let col = point.column.min(len);
        let offset = self.point_to_offset(Point::new(point.row, col));
        let clipped = self.clip_offset(offset, bias);
        self.offset_to_point(clipped)
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
        let utf16_col: u32 = text[line_start..scan_end]
            .chars()
            .map(|ch| ch.len_utf16() as u32)
            .sum();

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

        let text = chunk.text.as_str();
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

        let line_text = &text[line_start..];
        let mut utf16_count = 0u32;
        let mut byte_col = 0u32;
        for ch in line_text.chars() {
            if ch == '\n' || utf16_count >= remaining_utf16_col {
                break;
            }
            utf16_count += ch.len_utf16() as u32;
            byte_col += ch.len_utf8() as u32;
        }

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
        let utf16_delta: usize = chunk.text.as_str()[..remaining]
            .chars()
            .map(|ch| ch.len_utf16())
            .sum();

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
        let mut utf16_count = 0usize;
        let mut byte_offset = 0usize;
        for ch in chunk.text.as_str().chars() {
            if utf16_count >= remaining_utf16 {
                break;
            }
            utf16_count += ch.len_utf16();
            byte_offset += ch.len_utf8();
        }

        chunk_start_offset + byte_offset
    }

    pub fn clip_point_utf16(&self, point: PointUtf16, bias: Bias) -> PointUtf16 {
        let p = self.point_utf16_to_point(point);
        let clipped = self.clip_point(p, bias);
        self.point_to_point_utf16(clipped)
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
        let text = &chunk.text.as_str()[..remaining];
        let mut row_delta = 0u32;
        let mut utf16_col = 0u32;
        for ch in text.chars() {
            if ch == '\n' {
                row_delta += 1;
                utf16_col = 0;
            } else {
                utf16_col += ch.len_utf16() as u32;
            }
        }
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

        let text = chunk.text.as_str();
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

        let line_text = &text[line_start..];
        let mut utf16_count = 0u32;
        let mut byte_offset = 0usize;
        for ch in line_text.chars() {
            if ch == '\n' || utf16_count >= remaining_utf16_col {
                break;
            }
            utf16_count += ch.len_utf16() as u32;
            byte_offset += ch.len_utf8();
        }

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

fn nth_newline_offset_bitmap(newlines: Bitmap, n: u32) -> usize {
    let mut remaining = newlines;
    for _ in 0..n.saturating_sub(1) {
        remaining &= remaining.wrapping_sub(1);
    }
    remaining.trailing_zeros() as usize + 1
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
}
