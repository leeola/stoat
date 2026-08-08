//! The rope's conversions over the production bitmap width.
//!
//! `Bitmap` is `u128` in the built crate and narrowed to `u16` under the crate's
//! own `cfg(test)`, so that chunk boundaries stay reachable in unit tests. Two
//! Two paths therefore never run in that suite. `nth_set_bit`'s high-word
//! branch needs a set bit past position 64, and the wider lane assembly in
//! `chunk_bitmaps` only runs at the wider width. An integration test compiles
//! the crate as an ordinary dependency, without `cfg(test)`, which is the only
//! way to reach them.
//!
//! The reference models below duplicate ones that live inside the crate's unit
//! test module and cannot be imported from here. That duplication is what the
//! arrangement costs.

use stoat_text::{Bias, OffsetUtf16, Point, PointUtf16, Rope};

/// Several hundred bytes of mixed character widths, so chunks fill past byte 64
/// and the bitmap searches reach the high word.
///
/// The combining mark and the four-byte characters are what make the byte,
/// character and UTF-16 positions all disagree, which is the disagreement every
/// conversion below is about.
fn fixture() -> String {
    let mut text = String::new();
    for i in 0..24 {
        text.push_str("plain");
        text.push_str(&i.to_string());
        text.push(' ');
        text.push_str("e\u{301}");
        text.push_str("\u{4e16}\u{754c}");
        text.push('\u{1d11e}');
        text.push('\u{1f389}');
        text.push('\n');
    }
    text
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |i| i + 1)
}

fn reference_point(text: &str, offset: usize) -> Point {
    let row = text[..offset].matches('\n').count() as u32;
    Point::new(row, (offset - line_start(text, offset)) as u32)
}

fn reference_point_utf16(text: &str, offset: usize) -> PointUtf16 {
    let row = text[..offset].matches('\n').count() as u32;
    let column = text[line_start(text, offset)..offset]
        .chars()
        .map(|ch| ch.len_utf16() as u32)
        .sum();
    PointUtf16::new(row, column)
}

fn reference_offset_utf16(text: &str, offset: usize) -> OffsetUtf16 {
    OffsetUtf16(text[..offset].chars().map(char::len_utf16).sum())
}

fn reference_clip_offset(text: &str, offset: usize, bias: Bias) -> usize {
    let offset = offset.min(text.len());
    if text.is_char_boundary(offset) {
        return offset;
    }
    match bias {
        Bias::Left => (0..=offset).rev().find(|o| text.is_char_boundary(*o)),
        Bias::Right => (offset..=text.len()).find(|o| text.is_char_boundary(*o)),
    }
    .expect("a boundary exists in either direction")
}

/// A UTF-16 column naming the second unit of a surrogate pair resolves to
/// whichever end of that character the bias asks for.
fn reference_clip_point_utf16(text: &str, point: PointUtf16, bias: Bias) -> PointUtf16 {
    let rows = text.matches('\n').count() as u32;
    let row = point.row.min(rows);

    let start = if row == 0 {
        0
    } else {
        text.match_indices('\n')
            .nth(row as usize - 1)
            .map_or(text.len(), |(i, _)| i + 1)
    };
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset);
    let line = &text[start..end];

    let mut units = 0u32;
    let mut bytes = 0usize;
    for ch in line.chars() {
        if units >= point.column {
            break;
        }
        let char_start = bytes;
        units += ch.len_utf16() as u32;
        bytes += ch.len_utf8();
        if units > point.column {
            if matches!(bias, Bias::Left) {
                bytes = char_start;
            }
            break;
        }
    }

    let column = line[..bytes].chars().map(|ch| ch.len_utf16() as u32).sum();
    PointUtf16::new(row, column)
}

#[test]
fn conversions_agree_with_a_text_walk_at_the_production_bitmap_width() {
    let text = fixture();
    assert!(
        text.len() > 256,
        "the fixture has to span several chunks, not {}",
        text.len(),
    );
    let rope = Rope::from(text.as_str());

    for offset in 0..=text.len() {
        for bias in [Bias::Left, Bias::Right] {
            assert_eq!(
                rope.clip_offset(offset, bias),
                reference_clip_offset(&text, offset, bias),
                "clip_offset({offset}, {bias:?})",
            );
        }

        if !text.is_char_boundary(offset) {
            continue;
        }

        let point = reference_point(&text, offset);
        assert_eq!(
            rope.offset_to_point(offset),
            point,
            "offset_to_point({offset})"
        );
        assert_eq!(
            rope.point_to_offset(point),
            offset,
            "point_to_offset({point:?})"
        );

        let point_utf16 = reference_point_utf16(&text, offset);
        assert_eq!(
            rope.offset_to_point_utf16(offset),
            point_utf16,
            "offset_to_point_utf16({offset})",
        );
        assert_eq!(
            rope.point_utf16_to_offset(point_utf16),
            offset,
            "point_utf16_to_offset({point_utf16:?})",
        );

        let offset_utf16 = reference_offset_utf16(&text, offset);
        assert_eq!(
            rope.offset_to_offset_utf16(offset),
            offset_utf16,
            "offset_to_offset_utf16({offset})",
        );
        assert_eq!(
            rope.offset_utf16_to_offset(offset_utf16),
            offset,
            "offset_utf16_to_offset({offset_utf16:?})",
        );
    }
}

/// Clipping a UTF-16 column, over every column of every row.
///
/// Kept apart from the offset walk because it is the search that reaches the
/// bitmap's high word: resolving a column finds the nth set bit of a chunk's
/// code-unit map, and a column late in a filled chunk puts that bit past
/// position 64.
#[test]
fn clipping_utf16_columns_agrees_with_a_text_walk() {
    let text = fixture();
    let rope = Rope::from(text.as_str());

    let rows = text.matches('\n').count() as u32;
    for row in 0..=rows {
        // Past the longest line, so the clamp at the row's end is covered too.
        for column in 0..80u32 {
            let point = PointUtf16::new(row, column);
            for bias in [Bias::Left, Bias::Right] {
                assert_eq!(
                    rope.clip_point_utf16(point, bias),
                    reference_clip_point_utf16(&text, point, bias),
                    "clip_point_utf16({point:?}, {bias:?})",
                );
            }
        }
    }
}
