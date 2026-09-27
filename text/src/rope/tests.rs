use super::*;

/// The summed cell count against the walk it stands in for.
fn walked_cells(text: &str) -> u32 {
    text.chars().map(cell_width).sum()
}

/// Each of the three paths answers what the character walk answers.
///
/// The mask of undecided starts picks between them, so a chunk that lands
/// in the wrong one is a wrong count rather than a slow one.
#[test]
fn cells_agree_with_the_walk_on_every_path() {
    // Plain ASCII: nothing undecided, so two popcounts answer.
    let ascii = "let value = compute(index) + offset;\n".repeat(8);
    // One two-byte character among ASCII: the seek-per-start case, which a
    // whole-chunk decode is what this exists to avoid.
    let sprinkled = "caf\u{e9} au lait, served warm and plain\n".repeat(8);
    // Mostly multi-byte: a quarter of the bytes or more start a character
    // needing its own width, so the walk answers.
    let wide = "\u{4f60}\u{597d}\u{4e16}\u{754c}\n".repeat(8);
    // ASCII control bytes, which are undecided starts worth no cells.
    let controls = "a\u{7}b\u{1b}c\u{0}d\u{7f}e\n".repeat(8);
    // A tab and a newline beside an undecided start, so the decided
    // popcount and the seek have to agree on the same chunk.
    let mixed = "\tone\u{2014}two\n\tthree\u{7}four\n".repeat(8);

    for text in [&ascii, &sprinkled, &wide, &controls, &mixed] {
        let rope = Rope::from(text.as_str());
        assert_eq!(
            rope.summary().cells,
            walked_cells(text),
            "the summed count differs from the walk on {text:?}",
        );
    }
}

#[test]
fn cells_sum_across_chunks_and_through_wide_characters() {
    // Long enough to span chunks, and each piece holds a character the
    // popcount path cannot answer for: a wide one, a mark, and a tab.
    let text = "ab\u{4f60}c\u{301}d\te".repeat(MAX_BASE);
    let rope = Rope::from(text.as_str());

    assert!(rope.chunks.iter().count() > 1, "the text spans chunks");
    assert_eq!(
        rope.summary().cells,
        walked_cells(&text),
        "the summed count is what walking the characters gives",
    );
}

/// Stated as counts rather than against a walk, so the rule each character
/// is measured by is pinned and not only the agreement between two readings
/// of it.
#[test]
fn cells_measure_a_tab_a_newline_a_wide_character_and_a_mark() {
    assert_eq!(
        Rope::from("a\tb\nc").summary().cells,
        4,
        "three letters and a tab, the newline occupying no cell",
    );
    assert_eq!(
        Rope::from("a\u{4f60}b").summary().cells,
        4,
        "two letters and a wide character occupying two cells",
    );
    assert_eq!(
        Rope::from("e\u{301}x").summary().cells,
        2,
        "two letters, the combining mark occupying none",
    );
}

#[test]
fn a_cell_seek_answers_what_a_walk_would() {
    let text = "ab\u{4f60}c\u{301}d\te".repeat(MAX_BASE / 2);
    let rope = Rope::from(text.as_str());

    let mut cells = 0u32;
    for (offset, ch) in text.char_indices() {
        assert_eq!(
            rope.offset_to_cells(offset),
            cells,
            "the cells before byte {offset}",
        );
        // Not the offset itself. A character of no width shares its
        // predecessor's count, so a count can name several offsets.
        assert_eq!(
            rope.offset_to_cells(rope.cells_to_offset(cells, Bias::Left)),
            cells,
            "and seeking to {cells} cells lands at {cells} cells",
        );
        cells += cell_width(ch);
    }
    assert_eq!(
        rope.offset_to_cells(text.len()),
        rope.summary().cells,
        "the whole text's cells",
    );
}

#[test]
fn a_cell_seek_inside_a_wide_character_answers_by_bias() {
    let rope = Rope::from("a\u{4f60}b");

    assert_eq!(
        rope.cells_to_offset(2, Bias::Left),
        1,
        "the left bias stays at the wide character's start",
    );
    assert_eq!(
        rope.cells_to_offset(2, Bias::Right),
        4,
        "the right bias moves past it",
    );
}

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
        rope.clip_to_grapheme_boundaries_batch(&clips.iter().rev().copied().collect::<Vec<_>>()),
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

        let mut boundaries: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
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

/// Reference conversions written as plain char walks, and a randomized check
/// that the rope agrees with them.
///
/// The rope answers these from chunk bitmaps, which is fast but easy to get
/// subtly wrong at a chunk boundary or around a character that encodes to two
/// UTF-16 code units. These walk the text directly instead, so they are slow,
/// obviously correct, and independent of whatever representation the chunks
/// use.
mod utf16_reference {
    use super::super::*;

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
mod clip_reference {
    use super::super::*;

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
mod line_walk_tests {
    use super::super::*;

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
mod chunk_density_tests {
    use super::super::*;

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
mod edit_oracle_tests {
    use super::super::*;

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
mod summary_identity_tests {
    use super::super::*;

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
mod regex_cursor_tests {
    use super::super::Rope;
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
