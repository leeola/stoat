//! Conversions between LSP `Position` / `Range` and stoat byte offsets.
//!
//! LSP carries positions as `{ line, character }` where the meaning of
//! `character` depends on the negotiated [`OffsetEncoding`] (UTF-16 by
//! default, optionally UTF-8 or UTF-32). Stoat's [`Rope`] works in
//! UTF-8 byte offsets, so every LSP-driven action -- hover, goto,
//! rename, completion edits, diagnostics gutter, semantic tokens --
//! routes through these helpers to translate without off-by-one
//! errors on multi-byte characters.
//!
//! Spec invariants enforced here:
//! - A line index past the document end clips to EOF.
//! - A character index past a line's content clips to the line end, which is the position
//!   **before** the line terminator (LSP positions are line-end-character agnostic).
//! - An inverted [`Range`] (`start > end`) normalizes to an empty range at `end`, matching the
//!   vscode precedent that several language servers depend on.

use crate::host::OffsetEncoding;
use lsp_types::{Position, Range};
use std::ops::Range as ByteRange;
use stoat_text::{Bias, Point, PointUtf16, Rope};

/// Converts an LSP [`Position`] to a byte offset in `rope` per
/// `encoding`.
pub fn lsp_pos_to_byte_offset(rope: &Rope, pos: Position, encoding: OffsetEncoding) -> usize {
    if pos.line > rope.max_point().row {
        return rope.len();
    }

    match encoding {
        OffsetEncoding::Utf8 => {
            let clipped = rope.clip_point(Point::new(pos.line, pos.character), Bias::Left);
            rope.point_to_offset(clipped)
        },
        OffsetEncoding::Utf16 => {
            let clipped =
                rope.clip_point_utf16(PointUtf16::new(pos.line, pos.character), Bias::Left);
            rope.point_utf16_to_offset(clipped)
        },
        OffsetEncoding::Utf32 => {
            let line_start = rope.point_to_offset(Point::new(pos.line, 0));
            let consumed: usize = rope
                .chars_at(line_start)
                .take_while(|ch| *ch != '\n')
                .take(pos.character as usize)
                .map(char::len_utf8)
                .sum();
            line_start + consumed
        },
    }
}

/// Converts a byte offset in `rope` to an LSP [`Position`] per
/// `encoding`. Offsets past `rope.len()` clip to EOF.
pub fn byte_offset_to_lsp_pos(rope: &Rope, offset: usize, encoding: OffsetEncoding) -> Position {
    let offset = offset.min(rope.len());

    match encoding {
        OffsetEncoding::Utf8 => {
            let p = rope.offset_to_point(offset);
            Position::new(p.row, p.column)
        },
        OffsetEncoding::Utf16 => {
            let p = rope.offset_to_point_utf16(offset);
            Position::new(p.row, p.column)
        },
        OffsetEncoding::Utf32 => {
            let row = rope.offset_to_point(offset).row;
            let line_start = rope.point_to_offset(Point::new(row, 0));
            let target_within_line = offset - line_start;
            let mut byte_count = 0usize;
            let mut char_count = 0u32;
            for ch in rope.chars_at(line_start) {
                if byte_count >= target_within_line {
                    break;
                }
                byte_count += ch.len_utf8();
                char_count += 1;
            }
            Position::new(row, char_count)
        },
    }
}

/// Converts an LSP [`Range`] to a byte-offset range in `rope`.
pub fn lsp_range_to_byte_range(
    rope: &Rope,
    range: Range,
    encoding: OffsetEncoding,
) -> ByteRange<usize> {
    let (start, end) = if range.start > range.end {
        (range.end, range.end)
    } else {
        (range.start, range.end)
    };
    let start_offset = lsp_pos_to_byte_offset(rope, start, encoding);
    let end_offset = lsp_pos_to_byte_offset(rope, end, encoding);
    start_offset..end_offset
}

/// Converts a byte-offset range in `rope` to an LSP [`Range`].
pub fn byte_range_to_lsp_range(
    rope: &Rope,
    range: ByteRange<usize>,
    encoding: OffsetEncoding,
) -> Range {
    let start = byte_offset_to_lsp_pos(rope, range.start, encoding);
    let end = byte_offset_to_lsp_pos(rope, range.end, encoding);
    Range::new(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    const ENCODINGS: [OffsetEncoding; 3] = [
        OffsetEncoding::Utf8,
        OffsetEncoding::Utf16,
        OffsetEncoding::Utf32,
    ];

    #[test]
    fn empty_rope_maps_every_position_to_zero() {
        let r = rope("");
        for enc in ENCODINGS {
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 0), enc), 0);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, u32::MAX), enc), 0);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(u32::MAX, u32::MAX), enc), 0);
            assert_eq!(byte_offset_to_lsp_pos(&r, 0, enc), pos(0, 0));
            assert_eq!(byte_offset_to_lsp_pos(&r, 999, enc), pos(0, 0));
        }
    }

    #[test]
    fn single_line_ascii_clips_character_to_line_length() {
        let r = rope("hello");
        for enc in ENCODINGS {
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 0), enc), 0);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 3), enc), 3);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 5), enc), 5);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 99), enc), 5);
            assert_eq!(byte_offset_to_lsp_pos(&r, 3, enc), pos(0, 3));
            assert_eq!(byte_offset_to_lsp_pos(&r, 5, enc), pos(0, 5));
        }
    }

    #[test]
    fn multi_line_ascii_handles_line_terminators() {
        let r = rope("abc\ndef\n");
        for enc in ENCODINGS {
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 0), enc), 0);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 3), enc), 3);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(0, 99), enc), 3);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(1, 0), enc), 4);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(1, 3), enc), 7);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(2, 0), enc), 8);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(99, 0), enc), 8);

            assert_eq!(byte_offset_to_lsp_pos(&r, 0, enc), pos(0, 0));
            assert_eq!(byte_offset_to_lsp_pos(&r, 3, enc), pos(0, 3));
            assert_eq!(byte_offset_to_lsp_pos(&r, 4, enc), pos(1, 0));
            assert_eq!(byte_offset_to_lsp_pos(&r, 7, enc), pos(1, 3));
            assert_eq!(byte_offset_to_lsp_pos(&r, 8, enc), pos(2, 0));
        }
    }

    #[test]
    fn two_byte_utf8_char_widens_per_encoding() {
        let r = rope("a\u{00e9}b");

        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 1), OffsetEncoding::Utf8),
            1
        );
        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 3), OffsetEncoding::Utf8),
            3
        );
        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 2), OffsetEncoding::Utf8),
            1,
            "mid-codepoint UTF-8 byte clips to nearest boundary via Bias::Left",
        );

        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 1), OffsetEncoding::Utf16),
            1
        );
        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 2), OffsetEncoding::Utf16),
            3
        );

        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 1), OffsetEncoding::Utf32),
            1
        );
        assert_eq!(
            lsp_pos_to_byte_offset(&r, pos(0, 2), OffsetEncoding::Utf32),
            3
        );

        assert_eq!(
            byte_offset_to_lsp_pos(&r, 3, OffsetEncoding::Utf8),
            pos(0, 3)
        );
        assert_eq!(
            byte_offset_to_lsp_pos(&r, 3, OffsetEncoding::Utf16),
            pos(0, 2)
        );
        assert_eq!(
            byte_offset_to_lsp_pos(&r, 3, OffsetEncoding::Utf32),
            pos(0, 2)
        );
    }

    #[test]
    fn surrogate_pair_round_trips_to_b() {
        let r = rope("\u{1F600}b");
        let target_byte = 4;

        assert_eq!(
            byte_offset_to_lsp_pos(&r, target_byte, OffsetEncoding::Utf8),
            pos(0, 4),
        );
        assert_eq!(
            byte_offset_to_lsp_pos(&r, target_byte, OffsetEncoding::Utf16),
            pos(0, 2),
        );
        assert_eq!(
            byte_offset_to_lsp_pos(&r, target_byte, OffsetEncoding::Utf32),
            pos(0, 1),
        );

        for enc in ENCODINGS {
            let p = byte_offset_to_lsp_pos(&r, target_byte, enc);
            assert_eq!(lsp_pos_to_byte_offset(&r, p, enc), target_byte);
        }
    }

    #[test]
    fn lsp_range_normalizes_inverted_endpoints() {
        let r = rope("hello world");
        let inverted = Range::new(pos(0, 5), pos(0, 2));
        let byte_range = lsp_range_to_byte_range(&r, inverted, OffsetEncoding::Utf8);
        assert_eq!(byte_range, 2..2);
    }

    #[test]
    fn range_round_trip_preserves_byte_endpoints() {
        let r = rope("a\u{00e9}b\nc\u{1F600}d");
        let cases = [0..1, 1..3, 3..4, 4..5, 5..6, 6..10, 10..11, 0..11, 3..6];
        for enc in ENCODINGS {
            for case in &cases {
                let lsp_range = byte_range_to_lsp_range(&r, case.clone(), enc);
                let back = lsp_range_to_byte_range(&r, lsp_range, enc);
                assert_eq!(back, *case, "encoding={enc:?}, case={case:?}");
            }
        }
    }

    #[test]
    fn line_clipping_returns_eof_for_out_of_bounds_rows() {
        let r = rope("abc");
        for enc in ENCODINGS {
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(1, 0), enc), 3);
            assert_eq!(lsp_pos_to_byte_offset(&r, pos(99, 99), enc), 3);
        }
    }
}
