use crate::{
    sum_tree::{self, ContextLessSummary, Dimension},
    Bias, Item, OffsetUtf16, Point, SumTree,
};
use std::{cmp, ops::Range};

#[cfg(test)]
const MIN_BASE: usize = 8;
#[cfg(test)]
const MAX_BASE: usize = MIN_BASE * 2;

#[cfg(not(test))]
const MIN_BASE: usize = 64;
#[cfg(not(test))]
const MAX_BASE: usize = MIN_BASE * 2;

#[derive(Clone, Default, Debug)]
pub struct TextSummary {
    pub len: usize,
    pub len_utf16: OffsetUtf16,
    pub lines: Point,
}

impl ContextLessSummary for TextSummary {
    fn add_summary(&mut self, other: &Self) {
        self.len += other.len;
        self.len_utf16 += other.len_utf16;
        self.lines += other.lines;
    }
}

#[derive(Clone, Debug)]
struct Chunk(String);

impl Chunk {
    fn summarize(text: &str) -> TextSummary {
        let mut lines = Point::zero();
        let mut len_utf16 = OffsetUtf16(0);

        for ch in text.chars() {
            len_utf16.0 += ch.len_utf16();
            if ch == '\n' {
                lines.row += 1;
                lines.column = 0;
            } else {
                lines.column += ch.len_utf8() as u32;
            }
        }

        TextSummary {
            len: text.len(),
            len_utf16,
            lines,
        }
    }
}

impl Item for Chunk {
    type Summary = TextSummary;

    fn summary(&self, _cx: ()) -> TextSummary {
        Chunk::summarize(&self.0)
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
                let available = MAX_BASE.saturating_sub(last_chunk.0.len());
                if available > 0 && !text.is_empty() {
                    let mut take = cmp::min(available, text.len());
                    while take > 0 && !text.is_char_boundary(take) {
                        take -= 1;
                    }
                    if take > 0 {
                        last_chunk.0.push_str(&text[..take]);
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
            self.chunks.push(Chunk(chunk.to_string()), ());
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

    pub fn summary(&self) -> TextSummary {
        self.chunks.summary().clone()
    }

    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        ChunksIter {
            cursor: self.chunks.cursor::<usize>(()),
            started: false,
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
        self.cursor.item().map(|chunk| chunk.0.as_str())
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

    pub fn slice(&mut self, end_offset: usize) -> Rope {
        let mut slice = Rope::new();

        if let Some(chunk) = self.chunks.item() {
            let start_ix = self.offset - *self.chunks.start();
            let end_ix = end_offset.min(self.chunks.end()) - *self.chunks.start();
            if start_ix < end_ix {
                slice.push(&chunk.0[start_ix..end_ix]);
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
                    slice.push(&chunk.0[..end_ix]);
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

impl<'a> crate::SeekTarget<'a, TextSummary, usize> for usize {
    fn cmp(&self, cursor_location: &usize, _cx: ()) -> cmp::Ordering {
        Ord::cmp(self, cursor_location)
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
}
