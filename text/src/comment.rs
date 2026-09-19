use crate::Rope;
use std::ops::Range;

/// The content of `range`, trimmed of the whitespace at either end.
///
/// A block comment wraps the text rather than the selection, so a selection
/// that reaches past the code on either side still recognizes and produces the
/// same comment as one drawn tight around it.
///
/// `None` when the range holds nothing but whitespace, which is a range with no
/// text to comment.
pub fn trimmed_range(rope: &Rope, range: Range<usize>) -> Option<Range<usize>> {
    let mut start = range.start;
    for ch in rope.chars_at(range.start) {
        if start >= range.end || !ch.is_whitespace() {
            break;
        }
        start += ch.len_utf8();
    }
    if start >= range.end {
        return None;
    }

    let mut end = range.end;
    for ch in rope.reversed_chars_at(range.end) {
        if end <= start || !ch.is_whitespace() {
            break;
        }
        end -= ch.len_utf8();
    }
    Some(start..end)
}

/// Whether `range`'s text opens with `start` and closes with `end`.
///
/// Both have to be present, and the range has to be long enough to hold them
/// without their overlapping, so a bare `/*` is not read as a finished comment.
pub fn is_block_commented(rope: &Rope, range: Range<usize>, tokens: (&str, &str)) -> bool {
    let (start_token, end_token) = tokens;
    let Some(inner) = trimmed_range(rope, range) else {
        return false;
    };
    if inner.end - inner.start < start_token.len() + end_token.len() {
        return false;
    }

    spells_at(rope, inner.start, start_token)
        && spells_at(rope, inner.end - end_token.len(), end_token)
}

/// Whether the text of `rope` at `offset` spells `token`.
///
/// Compares chunk by chunk, so a long row is never copied to check a few bytes
/// at one end of it. A token that runs past the end of the rope does not match.
fn spells_at(rope: &Rope, offset: usize, token: &str) -> bool {
    let mut rest = token.as_bytes();
    for chunk in rope.chunks_in_range(offset..offset + token.len()) {
        let Some(tail) = rest.strip_prefix(chunk.as_bytes()) else {
            return false;
        };
        rest = tail;
    }
    rest.is_empty()
}

/// The edits that wrap `range`'s text in `tokens`, or unwrap it when it is
/// already wrapped.
///
/// Each edit is a byte range and its replacement, ordered by position. Callers
/// apply them back to front so the earlier offsets stay valid.
///
/// One space separates the tokens from the text, and uncommenting takes that
/// space back only where it is there. A comment written by hand without the
/// spaces therefore uncomments to exactly what it wrapped.
pub fn toggle_block_comment(
    rope: &Rope,
    range: Range<usize>,
    tokens: (&str, &str),
) -> Vec<(Range<usize>, String)> {
    let (start_token, end_token) = tokens;
    let Some(inner) = trimmed_range(rope, range.clone()) else {
        return Vec::new();
    };

    if !is_block_commented(rope, range, tokens) {
        return vec![
            (inner.start..inner.start, format!("{start_token} ")),
            (inner.end..inner.end, format!(" {end_token}")),
        ];
    }

    let len = inner.end - inner.start;
    let after_open = rope.chars_at(inner.start + start_token.len()).next();
    let before_close = rope.reversed_chars_at(inner.end - end_token.len()).next();
    let opened = start_token.len() + usize::from(after_open == Some(' '));

    // A comment holding nothing but its tokens has one space between them, so
    // both ends claim it and the second runs off the front of the range.
    let closing_space = before_close == Some(' ') && len > opened + end_token.len();
    let closed = end_token.len() + usize::from(closing_space);

    vec![
        (inner.start..inner.start + opened, String::new()),
        (inner.end - closed..inner.end, String::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(text: &str) -> Rope {
        Rope::from(text)
    }

    #[test]
    fn whitespace_around_the_range_is_not_part_of_it() {
        let r = rope("  let x = 1;  ");
        assert_eq!(trimmed_range(&r, 0..r.len()), Some(2..12));
    }

    #[test]
    fn a_range_of_only_whitespace_has_no_text() {
        let r = rope("    \n\t ");
        assert_eq!(trimmed_range(&r, 0..r.len()), None);
    }

    /// Both tokens have to be there, and far enough apart to be two of them.
    #[test]
    fn recognizing_a_block_comment() {
        let tokens = ("/*", "*/");
        for (text, expected) in [
            ("/* a */", true),
            ("/*a*/", true),
            ("  /* a */  ", true),
            ("/* a", false),
            ("a */", false),
            ("a", false),
            ("/*", false),
            ("/**/", true),
        ] {
            let r = rope(text);
            assert_eq!(
                is_block_commented(&r, 0..r.len(), tokens),
                expected,
                "{text:?}",
            );
        }
    }

    /// Applying the edits back to front, the way a caller does.
    fn toggled(text: &str, tokens: (&str, &str)) -> String {
        let r = rope(text);
        let mut out = text.to_string();
        for (range, replacement) in toggle_block_comment(&r, 0..r.len(), tokens)
            .into_iter()
            .rev()
        {
            out.replace_range(range, &replacement);
        }
        out
    }

    #[test]
    fn wrapping_and_unwrapping_round_trip() {
        let tokens = ("/*", "*/");
        assert_eq!(toggled("let x = 1;", tokens), "/* let x = 1; */");
        assert_eq!(toggled("/* let x = 1; */", tokens), "let x = 1;");
    }

    /// A comment written without the spaces gives up exactly its tokens, so the
    /// text inside it survives the round trip.
    #[test]
    fn unwrapping_takes_a_space_only_where_there_is_one() {
        let tokens = ("/*", "*/");
        assert_eq!(toggled("/*let x = 1;*/", tokens), "let x = 1;");
        assert_eq!(toggled("/* let x = 1;*/", tokens), "let x = 1;");
        assert_eq!(toggled("/*let x = 1; */", tokens), "let x = 1;");
    }

    /// The surrounding whitespace is not the comment's, so it stays put.
    #[test]
    fn wrapping_leaves_the_indentation_alone() {
        let tokens = ("/*", "*/");
        assert_eq!(toggled("    let x = 1;", tokens), "    /* let x = 1; */");
        assert_eq!(toggled("    /* let x = 1; */", tokens), "    let x = 1;");
    }

    #[test]
    fn an_empty_comment_unwraps_to_nothing() {
        let tokens = ("/*", "*/");
        assert_eq!(toggled("/**/", tokens), "");
        assert_eq!(toggled("/* */", tokens), "");
    }

    #[test]
    fn a_range_of_only_whitespace_produces_no_edits() {
        let r = rope("   ");
        assert_eq!(
            toggle_block_comment(&r, 0..r.len(), ("/*", "*/")),
            Vec::new()
        );
    }

    /// A row many rope chunks long, with its trailing whitespace running across
    /// chunks too, toggles the way a short one does. The whitespace stays
    /// outside the comment in both directions.
    #[test]
    fn a_long_row_with_trailing_spaces_toggles_like_a_short_one() {
        let tokens = ("/*", "*/");
        let code = "let x = 1;".repeat(1_000);
        let padding = " ".repeat(1_000);
        let plain = format!("{code}{padding}");
        let wrapped = format!("/* {code} */{padding}");

        assert_eq!(
            [toggled(&plain, tokens), toggled(&wrapped, tokens)],
            [wrapped, plain],
        );
    }

    /// At some offsets a token sits across the boundary between two rope
    /// chunks. Moving a comment right one column at a time takes each token
    /// across every position relative to a boundary.
    #[test]
    fn a_token_across_a_chunk_boundary_still_matches() {
        let tokens = ("/*", "*/");
        let unwrapped: Vec<String> = (0..300)
            .map(|indent| toggled(&format!("{}/* x */", " ".repeat(indent)), tokens))
            .collect();
        let expected: Vec<String> = (0..300)
            .map(|indent| format!("{}x", " ".repeat(indent)))
            .collect();
        assert_eq!(unwrapped, expected);
    }

    /// A different pair works the same way, since nothing about the algorithm
    /// is particular to the slash-star spelling.
    #[test]
    fn a_longer_token_pair_round_trips() {
        let tokens = ("<!--", "-->");
        assert_eq!(toggled("a b", tokens), "<!-- a b -->");
        assert_eq!(toggled("<!-- a b -->", tokens), "a b");
    }
}
