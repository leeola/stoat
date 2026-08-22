use ratatui::{buffer::Buffer, style::Style};

pub(crate) fn write_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if x < buf.area.x + buf.area.width && y < buf.area.y + buf.area.height {
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn write_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn write_str_clipped(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    s: &str,
    style: Style,
    end_x: u16,
) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= end_x || col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

/// Wrap a line of styled spans to `width` columns, keeping each piece's style.
///
/// [`wrap_text`] cannot take this. A break has to fall between words *across*
/// span boundaries, and a word split by a span change ("`Foo`bar") is still one
/// word, so wrapping each span alone breaks in the wrong places and loses the
/// styles besides.
///
/// A single word wider than `width` gets a line of its own rather than being
/// split. Code spans are the usual case, and a broken identifier is worse to
/// read than one that overruns.
///
/// An empty input line yields one empty output line, so a blank line between
/// paragraphs survives the wrap.
pub(crate) fn wrap_styled(line: &[(String, Style)], width: usize) -> Vec<Vec<(String, Style)>> {
    if width == 0 {
        return Vec::new();
    }
    if line.iter().all(|(text, _)| text.is_empty()) {
        return vec![Vec::new()];
    }

    let mut lines = Vec::new();
    let mut current: Vec<(String, Style)> = Vec::new();
    let mut current_w = 0usize;

    for word in styled_words(line) {
        let word_w: usize = word.iter().map(|(text, _)| text.chars().count()).sum();
        let needs_space = current_w > 0;
        let add_w = word_w + usize::from(needs_space);

        if current_w > 0 && current_w + add_w > width {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }

        if current_w > 0 {
            // The space joins the preceding piece, so a run of same-styled
            // words stays one span rather than one per word.
            if let Some((text, _)) = current.last_mut() {
                text.push(' ');
            }
            current_w += 1;
        }
        for (text, style) in word {
            match current.last_mut() {
                Some((last, last_style)) if *last_style == style => last.push_str(&text),
                _ => current.push((text, style)),
            }
        }
        current_w += word_w;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Split styled spans into words, each word carrying the styled pieces it is
/// made of.
///
/// A word that crosses a span boundary comes back as several pieces, which is
/// what keeps "`Foo`bar" one word with its code half still styled.
fn styled_words(line: &[(String, Style)]) -> Vec<Vec<(String, Style)>> {
    let mut words: Vec<Vec<(String, Style)>> = Vec::new();
    // Whether the previous span ended mid-word, so the next span's first piece
    // joins it rather than starting a word of its own.
    let mut open = false;

    for (text, style) in line {
        let mut pieces = text.split_whitespace();
        let leading_space = text.starts_with(char::is_whitespace);

        if let Some(first) = pieces.next() {
            match (open && !leading_space, words.last_mut()) {
                (true, Some(word)) => word.push((first.to_owned(), *style)),
                _ => words.push(vec![(first.to_owned(), *style)]),
            }
        }
        for piece in pieces {
            words.push(vec![(piece.to_owned(), *style)]);
        }

        open = !text.is_empty() && !text.ends_with(char::is_whitespace);
    }

    words
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let trimmed_start = text.trim_start();
    if trimmed_start.is_empty() {
        return Vec::new();
    }
    let indent_byte_len = text.len() - trimmed_start.len();
    let indent = text[..indent_byte_len].to_string();
    let indent_w = indent.chars().count();
    if indent_w >= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = indent.clone();
    let mut current_w = indent_w;
    for word in trimmed_start.split_whitespace() {
        let needs_space = current_w > indent_w;
        let word_w = word.chars().count();
        let add_w = word_w + usize::from(needs_space);
        if current_w + add_w <= width {
            if needs_space {
                current.push(' ');
            }
            current.push_str(word);
            current_w += add_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current = indent.clone();
            current.push_str(word);
            current_w = indent_w + word_w;
        }
    }
    if current_w > indent_w {
        lines.push(current);
    }
    lines
}

/// Cut `line` to at most `width` characters.
///
/// Counts characters, not display columns, so a wide glyph still counts once.
/// The popups that use this lay their text out in character positions, which
/// keeps the truncation in the same units as the layout. See
/// [`truncate_to_cols`] for the display-column counterpart.
pub(crate) fn truncate_to_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
}

/// Borrow the first `width` characters of `line`.
///
/// The borrowing counterpart of [`truncate_to_width`], for a caller that only
/// paints the result and has no use for a string of its own. The cut lands on
/// the byte index of the first character past `width`, which is what keeps the
/// result a borrow. Taking `width` characters instead collects them.
///
/// Counts characters, not display columns, so a wide glyph still counts once.
pub(crate) fn clip_to_width(line: &str, width: usize) -> &str {
    match line.char_indices().nth(width) {
        Some((end, _)) => &line[..end],
        None => line,
    }
}

pub(crate) fn truncate_to_cols(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cols {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{clip_to_width, wrap_styled};
    use ratatui::style::Style;

    /// The clip is a byte slice of its input, so a width landing inside a
    /// multi-byte character has to cut on the character boundary rather than
    /// the byte the count arrives at. Getting that wrong panics rather than
    /// merely painting the wrong text.
    #[test]
    fn a_clip_cuts_on_a_character_boundary() {
        assert_eq!(clip_to_width("héllo", 3), "hél", "counts characters");
        assert_eq!(clip_to_width("héllo", 0), "", "an empty clip is empty");
        assert_eq!(clip_to_width("hé", 9), "hé", "a short line is whole");
        assert_eq!(clip_to_width("hé", 2), "hé", "and so is an exact fit");
    }

    /// The narration arrives as styled spans, so a break has to fall between
    /// words across a span boundary rather than inside one span at a time.
    #[test]
    fn a_wrap_breaks_between_words_across_a_span_boundary() {
        let code = Style::default().fg(ratatui::style::Color::Red);
        let line = vec![
            ("the quick ".to_owned(), Style::default()),
            ("brown".to_owned(), code),
            (" fox jumps".to_owned(), Style::default()),
        ];

        let wrapped = wrap_styled(&line, 15);
        let texts: Vec<String> = wrapped
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
            .collect();

        assert_eq!(texts, ["the quick brown", "fox jumps"]);
        assert!(
            wrapped[0]
                .iter()
                .any(|(text, style)| text == "brown" && *style == code),
            "and the code span keeps its style, got {wrapped:?}",
        );
    }

    /// A word split by a span change is still one word. Wrapping each span on
    /// its own would break it in the middle and read as two.
    #[test]
    fn a_word_crossing_a_span_boundary_stays_whole() {
        let code = Style::default().fg(ratatui::style::Color::Red);
        let line = vec![
            ("aaa ".to_owned(), Style::default()),
            ("Foo".to_owned(), code),
            ("bar baz".to_owned(), Style::default()),
        ];

        let wrapped = wrap_styled(&line, 8);
        let texts: Vec<String> = wrapped
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
            .collect();

        assert_eq!(texts, ["aaa", "Foobar", "baz"], "Foobar never split");
    }

    /// A code span longer than the card overruns rather than being cut. A
    /// broken identifier is harder to read than one that runs past the edge.
    #[test]
    fn a_word_wider_than_the_width_takes_its_own_line() {
        let line = vec![
            ("see ".to_owned(), Style::default()),
            ("a_very_long_identifier_name".to_owned(), Style::default()),
            (" here".to_owned(), Style::default()),
        ];

        let texts: Vec<String> = wrap_styled(&line, 10)
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
            .collect();

        assert_eq!(texts, ["see", "a_very_long_identifier_name", "here"]);
    }

    /// A blank line between paragraphs is what separates them, so it survives
    /// the wrap rather than collapsing.
    #[test]
    fn a_blank_line_survives_the_wrap() {
        assert_eq!(wrap_styled(&[], 20), vec![Vec::new()]);
        assert_eq!(
            wrap_styled(&[(String::new(), Style::default())], 20),
            vec![Vec::new()],
        );
    }
}
