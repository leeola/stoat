use std::borrow::Cow;

/// Bytes of a file's leading run the detector reads before giving up.
///
/// A file states its line ending on its first line, so scanning further only
/// costs time. The bound is in bytes rather than lines because a file with no
/// terminator at all would otherwise be read end to end to learn nothing.
const SCAN_BYTES: usize = 1000;

/// The line terminator a file on disk uses.
///
/// A buffer always holds its text in LF regardless of this, so nothing between
/// the read and the write has to reason about carriage returns. The value is
/// remembered only so the file can be written back in the form it arrived in.
///
/// The [`Default`] is [`LineEnding::Lf`], which is what a file carrying no
/// terminator at all gets, and what a buffer with no file behind it starts as.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    /// The ending `text` uses, taken from its first line terminator.
    ///
    /// Only the leading [`SCAN_BYTES`] are read. A file whose first terminator
    /// lies past that, or which has none, reads as [`LineEnding::Lf`].
    pub fn detect(text: &str) -> Self {
        let mut end = text.len().min(SCAN_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        match text[..end].find('\n') {
            Some(0) | None => Self::Lf,
            Some(ix) if text.as_bytes()[ix - 1] == b'\r' => Self::Crlf,
            Some(_) => Self::Lf,
        }
    }

    /// Rewrite every line terminator in `text` as a bare `\n`.
    ///
    /// Both `\r\n` and a lone `\r` are terminators, so a file mixing the two
    /// arrives uniform. Text already free of carriage returns is borrowed
    /// rather than copied, which is the ordinary case.
    pub fn normalize(text: &str) -> Cow<'_, str> {
        if !text.contains('\r') {
            return Cow::Borrowed(text);
        }

        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(ix) = rest.find('\r') {
            out.push_str(&rest[..ix]);
            out.push('\n');
            // A `\r\n` is one terminator, so the `\n` it carries is consumed
            // with it. A lone `\r` is a terminator on its own.
            let consumed = if rest[ix + 1..].starts_with('\n') {
                2
            } else {
                1
            };
            rest = &rest[ix + consumed..];
        }
        out.push_str(rest);
        Cow::Owned(out)
    }

    /// Rewrite the bare `\n` terminators of a normalized `text` back into this
    /// ending.
    ///
    /// LF borrows. Expects text that went through [`Self::normalize`], so any
    /// `\r` still present is treated as ordinary content rather than as part of
    /// a terminator.
    pub fn restore<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self {
            Self::Lf => Cow::Borrowed(text),
            Self::Crlf => Cow::Owned(text.replace('\n', "\r\n")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineEnding;

    #[test]
    fn detection_reads_the_first_terminator() {
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(LineEnding::detect("a\nb\n"), LineEnding::Lf);
        // A later CRLF does not override an LF that came first.
        assert_eq!(LineEnding::detect("a\nb\r\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("no terminator"), LineEnding::Lf);
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
        // A leading newline has no byte before it to inspect.
        assert_eq!(LineEnding::detect("\na"), LineEnding::Lf);
    }

    #[test]
    fn detection_gives_up_past_the_scan_bound() {
        let far = format!("{}\r\n", "x".repeat(super::SCAN_BYTES));
        assert_eq!(LineEnding::detect(&far), LineEnding::Lf);
    }

    #[test]
    fn detection_stops_on_a_char_boundary() {
        // A multi-byte char straddling the bound must not split mid-sequence.
        let text = format!("{}\u{e9}\r\n", "x".repeat(super::SCAN_BYTES - 1));
        assert_eq!(LineEnding::detect(&text), LineEnding::Lf);
    }

    #[test]
    fn normalize_flattens_every_terminator() {
        assert_eq!(LineEnding::normalize("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(LineEnding::normalize("a\rb\r"), "a\nb\n");
        assert_eq!(LineEnding::normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn normalize_borrows_text_without_carriage_returns() {
        assert!(matches!(
            LineEnding::normalize("a\nb\n"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn crlf_round_trips_through_normalize_and_restore() {
        let original = "one\r\ntwo\r\nthree\r\n";
        let normalized = LineEnding::normalize(original);
        assert_eq!(normalized, "one\ntwo\nthree\n");
        assert_eq!(LineEnding::Crlf.restore(&normalized), original);
        assert_eq!(LineEnding::Lf.restore(&normalized), "one\ntwo\nthree\n");
    }
}
