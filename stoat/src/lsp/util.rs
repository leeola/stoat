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

use crate::{buffer_registry::BufferRegistry, host::OffsetEncoding};
use lsp_types::{Position, Range, Uri};
use percent_encoding::{percent_decode_str, percent_encode, AsciiSet, CONTROLS};
use std::{
    ops::Range as ByteRange,
    path::{Path, PathBuf},
};
use stoat_text::{Bias, Point, PointUtf16, Rope};

/// The bytes a URI path may not carry literally.
///
/// This is the complement of what RFC 3986 allows in a path, which is more than
/// the unreserved characters. The sub-delims, `:` and `@` are legal there too,
/// and `/` separates segments. Encoding only what is actually illegal leaves an
/// ordinary path's URI byte-identical to the path, so a server matching URIs
/// against what it sent goes on recognizing them.
///
/// `%` is in the set, so a literal percent in a filename survives the round trip
/// instead of reading back as an escape.
const URI_PATH_ILLEGAL: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// The most rows without a position that the batch conversion steps over.
///
/// A longer gap restarts the walk with a descent. A step over one row costs
/// about a twentieth of a restart, which takes three tree descents.
const MAX_STEPPED_ROWS: u32 = 20;

/// Percent-encode `path` for the path component of a `file:` URI.
///
/// Without this a path holding a space or any non-ASCII character makes a
/// string no URI parser accepts, and the file it names ends up with no language
/// server at all.
pub(crate) fn percent_encode_path(path: &str) -> String {
    percent_encode(path.as_bytes(), URI_PATH_ILLEGAL).to_string()
}

/// The filesystem path `uri`'s path component names, undoing
/// [`percent_encode_path`].
///
/// A percent sequence decoding to bytes that are not UTF-8 is replaced rather
/// than rejected. Stoat has no path for those to become: building a URI needs a
/// path that is valid UTF-8 to start with, so the replaced path matches no
/// buffer, which is where rejecting it would have led anyway.
pub(crate) fn percent_decode_path(uri: &Uri) -> PathBuf {
    PathBuf::from(
        percent_decode_str(uri.path().as_str())
            .decode_utf8_lossy()
            .as_ref(),
    )
}

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

/// Convert LSP [`Position`]s to byte offsets in `rope`, in the input order.
///
/// Each answer is what [`lsp_pos_to_byte_offset`] gives for the same position.
/// The conversion answers the positions in line order from one forward walk
/// over the rows they name. A token stream therefore costs about one pass over
/// its rows rather than a descent per token. A row far past the one before it
/// restarts the walk, so a few positions spread over a large buffer cost a
/// restart each, not a pass over all of it.
pub fn lsp_positions_to_byte_offsets_batch(
    rope: &Rope,
    positions: &[Position],
    encoding: OffsetEncoding,
) -> Vec<usize> {
    let key = |pos: &Position| (pos.line, pos.character);
    let mut order: Vec<usize> = (0..positions.len()).collect();
    if !positions.is_sorted_by_key(key) {
        order.sort_unstable_by_key(|&index| key(&positions[index]));
    }

    let mut offsets = vec![0; positions.len()];
    let mut text = String::new();
    let mut walk = None;
    for run in order.chunk_by(|&a, &b| positions[a].line == positions[b].line) {
        let row = positions[run[0]].line;
        let (mut rows, next_row, mut start) = match walk.take() {
            Some((rows, next_row, start)) if row - next_row <= MAX_STEPPED_ROWS => {
                (rows, next_row, start)
            },
            _ => (
                rope.line_walk(row..u32::MAX),
                row,
                rope.point_to_offset(Point::new(row, 0)),
            ),
        };
        for _ in next_row..row {
            let Some((_, len)) = rows.next_len() else {
                break;
            };
            start += len as usize + 1;
        }

        text.clear();
        let Some(len) = rows.next_into(&mut text) else {
            // A row past the last clips to the rope's end.
            for &index in run {
                offsets[index] = rope.len();
            }
            continue;
        };
        fill_row_offsets(&text, start, run, positions, encoding, &mut offsets);
        walk = Some((rows, row + 1, start + len as usize + 1));
    }
    offsets
}

/// Writes the offset of each position in `run`, which all name the row `line`
/// that starts at byte `start`.
///
/// A column clips to the last character boundary at or before it, and to the
/// row's end. The run ascends, so one walk along the row answers all of it.
fn fill_row_offsets(
    line: &str,
    start: usize,
    run: &[usize],
    positions: &[Position],
    encoding: OffsetEncoding,
    offsets: &mut [usize],
) {
    // An ASCII byte is one unit in every encoding, so a column on an ASCII row
    // is its byte count.
    if line.is_ascii() {
        for &index in run {
            offsets[index] = start + line.len().min(positions[index].character as usize);
        }
        return;
    }

    let width = |ch: char| match encoding {
        OffsetEncoding::Utf8 => ch.len_utf8(),
        OffsetEncoding::Utf16 => ch.len_utf16(),
        OffsetEncoding::Utf32 => 1,
    };
    let mut chars = line.chars().peekable();
    let (mut column, mut bytes) = (0, 0);
    for &index in run {
        let target = positions[index].character as usize;
        while let Some(&ch) = chars.peek()
            && column + width(ch) <= target
        {
            column += width(ch);
            bytes += ch.len_utf8();
            chars.next();
        }
        offsets[index] = start + bytes;
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

/// Anchor each of `published`'s ranges into the buffer for `path`, converting
/// through `encoding`.
///
/// Anchored here, where the publish has just landed and the ranges still name
/// the text the server measured, rather than every frame against text that has
/// moved since. From here the fragment tree carries each mark along with the
/// text it sits on, so a reader resolves instead of replaying edits. A path with
/// no open buffer has nothing to anchor into and yields unresolved spans.
///
/// Starts take [`Bias::Right`] and ends [`Bias::Left`], so text inserted at
/// either edge falls outside the mark rather than widening it.
pub(crate) fn publish_spans(
    path: &Path,
    published: &[lsp_types::Diagnostic],
    encoding: OffsetEncoding,
    buffers: &BufferRegistry,
) -> Vec<crate::diagnostics::PublishedSpan> {
    let Some(buffer) = buffers.id_for_path(path).and_then(|id| buffers.get(id)) else {
        return vec![crate::diagnostics::PublishedSpan::unresolved(); published.len()];
    };
    let guard = buffer.read().expect("buffer lock");
    let rope = guard.rope();

    // An inverted range starts at its end, as `lsp_range_to_byte_range` reads it.
    let endpoints: Vec<Position> = published
        .iter()
        .flat_map(|diag| [diag.range.start.min(diag.range.end), diag.range.end])
        .collect();
    let offsets = lsp_positions_to_byte_offsets_batch(rope, &endpoints, encoding);
    let starts: Vec<usize> = offsets.iter().step_by(2).copied().collect();
    let ends: Vec<usize> = offsets.iter().skip(1).step_by(2).copied().collect();

    let snapshot = &guard.snapshot;
    snapshot
        .anchors_at_batch(&starts, Bias::Right)
        .into_iter()
        .zip(snapshot.anchors_at_batch(&ends, Bias::Left))
        .map(|pair| crate::diagnostics::PublishedSpan {
            anchors: Some(pair),
        })
        .collect()
}

/// Convert an LSP `file:` URI to a [`PathBuf`]. Returns `None` for any
/// other scheme; non-`file:` diagnostic notifications are silently
/// dropped because stoat has no concept of remote-path buffers today.
pub(crate) fn lsp_uri_to_path(uri: &Uri) -> Option<PathBuf> {
    if uri.scheme().map(|s| s.as_str()) != Some("file") {
        return None;
    }
    Some(percent_decode_path(uri))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::Stoat, test_fixture::open_indent_buffer};

    #[test]
    fn publish_spans_resolve_in_the_publishing_servers_encoding() {
        use crate::host::OffsetEncoding;
        use lsp_types::{Diagnostic, Position, Range as LspRange};

        let mut h = Stoat::test();
        // é is two UTF-8 bytes but one UTF-16 unit, so character 2 is byte 2
        // under UTF-8 and byte 3 under UTF-16.
        open_indent_buffer(&mut h, "a.txt", "\u{e9}xy\n".as_bytes());
        let path = PathBuf::from("/indent/a.txt");

        let diag = Diagnostic::new_simple(
            LspRange::new(Position::new(0, 2), Position::new(0, 3)),
            "boom".to_string(),
        );
        let buffers = &h.stoat.active_workspace().buffers;
        let utf8 = publish_spans(
            &path,
            std::slice::from_ref(&diag),
            OffsetEncoding::Utf8,
            buffers,
        );
        let utf16 = publish_spans(&path, &[diag], OffsetEncoding::Utf16, buffers);

        // A span is anchored rather than stored as offsets, so reading one back
        // resolves it against the buffer it was taken in.
        let buffer = buffers
            .id_for_path(&path)
            .and_then(|id| buffers.get(id))
            .expect("open buffer");
        let snapshot = buffer.read().expect("poisoned").snapshot.clone();
        let offsets = |span: &crate::diagnostics::PublishedSpan| {
            let (start, end) = span.anchors.expect("the buffer is open, so it anchored");
            snapshot.resolve_anchor(&start)..snapshot.resolve_anchor(&end)
        };

        assert_eq!(offsets(&utf8[0]), 2..3, "utf-8 character 2 is the x");
        assert_eq!(offsets(&utf16[0]), 3..4, "utf-16 character 2 is the y");
    }

    /// Typing against either edge of a marked span has to leave the mark on the
    /// text the server named rather than stretch it over what was just typed.
    /// The biases the publish anchors with are what decide that.
    #[test]
    fn publish_spans_anchor_typing_at_an_edge_outside_the_mark() {
        use crate::host::OffsetEncoding;
        use lsp_types::{Diagnostic, Position, Range as LspRange};

        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.txt", b"alpha bravo\n");
        let path = PathBuf::from("/indent/a.txt");

        // `bravo` is [6, 11).
        let diag = Diagnostic::new_simple(
            LspRange::new(Position::new(0, 6), Position::new(0, 11)),
            "boom".to_string(),
        );
        let buffers = &h.stoat.active_workspace().buffers;
        let spans = publish_spans(&path, &[diag], OffsetEncoding::Utf16, buffers);
        let (start, end) = spans[0]
            .anchors
            .expect("the buffer is open, so it anchored");

        let buffer = buffers
            .id_for_path(&path)
            .and_then(|id| buffers.get(id))
            .expect("open buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(11..11, "Z");
            guard.edit(6..6, "Y");
        }
        let after = buffer.read().expect("poisoned").snapshot.clone();

        assert_eq!(
            after.resolve_anchor(&start)..after.resolve_anchor(&end),
            7..12,
            "the mark covers `bravo` alone, the Y before it and the Z after",
        );
    }

    /// A range whose start follows its end publishes as the empty span at its
    /// end, which is how [`lsp_range_to_byte_range`] reads such a range.
    #[test]
    fn publish_spans_read_an_inverted_range_as_empty_at_its_end() {
        use lsp_types::Diagnostic;

        let mut h = Stoat::test();
        open_indent_buffer(&mut h, "a.txt", b"alpha bravo\n");
        let path = PathBuf::from("/indent/a.txt");

        let diag = Diagnostic::new_simple(Range::new(pos(0, 11), pos(0, 6)), "boom".to_string());
        let buffers = &h.stoat.active_workspace().buffers;
        let spans = publish_spans(&path, &[diag], OffsetEncoding::Utf16, buffers);
        let (start, end) = spans[0]
            .anchors
            .expect("the buffer is open, so it anchored");

        let buffer = buffers
            .id_for_path(&path)
            .and_then(|id| buffers.get(id))
            .expect("open buffer");
        let snapshot = buffer.read().expect("poisoned").snapshot.clone();
        assert_eq!(
            snapshot.resolve_anchor(&start)..snapshot.resolve_anchor(&end),
            6..6,
            "the span is empty at the range's end",
        );
    }

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    /// The path a URI built from `path` reads back as, which is the whole round
    /// trip a buffer's identity depends on.
    fn round_trip(path: &str) -> PathBuf {
        let uri = crate::action_handlers::lsp::path_to_uri(Path::new(path))
            .expect("path makes a valid uri");
        percent_decode_path(&uri)
    }

    #[test]
    fn a_path_survives_the_uri_round_trip() {
        for path in [
            "/ws/plain.rs",
            "/ws/my file.rs",
            "/ws/\u{65e5}\u{672c}.rs",
            "/ws/100% done.rs",
            "/ws/a#b.rs",
            "/ws/we[i]rd.rs",
        ] {
            assert_eq!(round_trip(path), PathBuf::from(path), "{path}");
        }
    }

    #[test]
    fn a_legal_path_is_not_encoded_at_all() {
        // Every one of these is a pchar, so the uri stoat sends for them is
        // unchanged and a server matching what it sent still recognizes it.
        let path = "/ws/a:b@c!d$e&f'g(h)i*j+k,l;m=n-o.p_q~r.rs";
        let uri = crate::action_handlers::lsp::path_to_uri(Path::new(path))
            .expect("path makes a valid uri");
        assert_eq!(uri.as_str(), format!("file://{path}"));
    }

    #[test]
    fn a_percent_escaped_uri_names_the_unescaped_path() {
        let uri: Uri = "file:///ws/my%20file.rs".parse().expect("valid uri");
        assert_eq!(percent_decode_path(&uri), PathBuf::from("/ws/my file.rs"));
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

    /// Literal offsets rather than agreement with the single-position path, so
    /// the batch's own contract does not rest on the two being the same code.
    #[test]
    fn positions_batch_answers_in_input_order() {
        // "he" then a two-byte e-acute, a newline, then a four-byte emoji and
        // "ok". Byte offsets: h=0 e=1 é=2..4 \n=4 emoji=5..9 o=9 k=10.
        let r = rope("he\u{00e9}\n\u{1F600}ok");

        // Row 1 column 2 is past the emoji under UTF-16, which spends two of
        // its units, and past its first byte under UTF-8.
        let positions = [pos(1, 2), pos(0, 0), pos(0, 3), pos(1, 0)];
        assert_eq!(
            lsp_positions_to_byte_offsets_batch(&r, &positions, OffsetEncoding::Utf16),
            vec![9, 0, 4, 5],
            "utf16 offsets, in the order asked for"
        );
        assert_eq!(
            lsp_positions_to_byte_offsets_batch(&r, &positions, OffsetEncoding::Utf32),
            vec![10, 0, 4, 5],
            "utf32 counts the emoji as one, so column two clears it and the o"
        );
    }

    /// The batch must answer what the single-position conversion does. The cases
    /// are every character boundary, a column inside a multibyte character, a
    /// column past a line's end, and rows past the last. The positions come in
    /// no particular order, and the walk reaches their rows both by steps and by
    /// restarts.
    #[test]
    fn positions_batch_matches_per_token() {
        // The walk restarts at `far` after row 9 and steps over every other gap.
        let far = 10 + MAX_STEPPED_ROWS + 1;
        let mut text: String = ["h\u{00e9}llo\n", "\u{1F600} tail\n", "plain\n"]
            .iter()
            .cycle()
            .take(far as usize + 5)
            .copied()
            .collect();
        text.push_str("last line");
        let r = rope(&text);
        for enc in ENCODINGS {
            let mut positions: Vec<Position> = std::iter::once(0)
                .chain(text.char_indices().map(|(i, ch)| i + ch.len_utf8()))
                .map(|off| byte_offset_to_lsp_pos(&r, off, enc))
                .filter(|p| [0, 1, 2, 5, 9, far, far + 3, far + 5].contains(&p.line))
                .collect();
            // Inside the e-acute's two bytes, inside the emoji's two UTF-16
            // units, past the ends of a non-ASCII and an ASCII line, a step
            // past the last line, and far past it.
            positions.extend([
                pos(0, 2),
                pos(1, 1),
                pos(0, 999),
                pos(2, 999),
                pos(far + 7, 0),
                pos(99, 0),
            ]);
            positions.reverse();
            let third = positions.len() / 3;
            positions.rotate_left(third);

            let batch = lsp_positions_to_byte_offsets_batch(&r, &positions, enc);
            let single: Vec<usize> = positions
                .iter()
                .map(|&p| lsp_pos_to_byte_offset(&r, p, enc))
                .collect();
            assert_eq!(batch, single, "encoding {enc:?}");
            assert_eq!(
                lsp_positions_to_byte_offsets_batch(&r, &[], enc),
                Vec::<usize>::new(),
                "an empty batch under {enc:?}",
            );
        }
    }
}
