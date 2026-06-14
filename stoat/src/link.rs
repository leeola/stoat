use regex::Regex;
use std::{ops::Range, sync::LazyLock};
use stoat_text::{Point, Rope};

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s]+").expect("valid url regex"));

static PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([^\s:]+)(?::(\d+)(?::(\d+))?)?").expect("valid path regex"));

/// A clickable target resolved from buffer or terminal text: an
/// `http`/`https` URL, or a file path carrying an optional line and
/// column.
///
/// Produced by [`detect_link`] over editor buffers and by the terminal
/// grid's link scan, and consumed by the open-routing layer that turns
/// one into a browser launch or a buffer open. Sharing one type keeps
/// both surfaces speaking the same link vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// An `http`/`https` URL.
    Url(String),
    /// A file path with an optional `line` and `column` parsed from a
    /// trailing `:line` / `:line:col` suffix.
    Path {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

/// Detect a clickable link covering `offset` in `rope`.
///
/// Scans only the line containing `offset` and returns the first target
/// whose span covers it: an `http`/`https` URL, or a whitespace-bounded
/// token that looks like a path (contains `/` or `.`) with an optional
/// `:line` or `:line:col` suffix. URLs take precedence over paths. The
/// returned range is in absolute buffer byte offsets, suitable for
/// highlighting the span or hit-testing a click.
///
/// Returns `None` when no link covers `offset`.
pub fn detect_link(rope: &Rope, offset: usize) -> Option<(Range<usize>, LinkTarget)> {
    let row = rope.offset_to_point(offset).row;
    let line_start = rope.point_to_offset(Point::new(row, 0));
    let local = offset.checked_sub(line_start)?;

    let line = rope.line_at_row(row);
    let (range, target) = detect_link_in_line(&line, local)?;

    Some((line_start + range.start..line_start + range.end, target))
}

/// Detect a link covering byte offset `local` within a single line of
/// text, returning the match's line-local byte range.
fn detect_link_in_line(line: &str, local: usize) -> Option<(Range<usize>, LinkTarget)> {
    for m in URL_RE.find_iter(line) {
        if (m.start()..m.end()).contains(&local) {
            return Some((m.start()..m.end(), LinkTarget::Url(m.as_str().to_string())));
        }
    }

    for cap in PATH_RE.captures_iter(line) {
        let whole = cap.get(0).expect("group 0 is always present");
        if !(whole.start()..whole.end()).contains(&local) {
            continue;
        }

        let path = cap.get(1).expect("path group is always present").as_str();
        if !path.contains('/') && !path.contains('.') {
            return None;
        }

        let line_no = cap.get(2).and_then(|m| m.as_str().parse().ok());
        let column = cap.get(3).and_then(|m| m.as_str().parse().ok());
        return Some((
            whole.start()..whole.end(),
            LinkTarget::Path {
                path: path.to_string(),
                line: line_no,
                column,
            },
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset_of(text: &str, needle: &str) -> usize {
        text.find(needle).expect("needle present in text")
    }

    #[test]
    fn detects_url_under_offset() {
        let text = "see https://example.com/a here";
        let (range, target) =
            detect_link(&Rope::from(text), offset_of(text, "example")).expect("link");
        assert_eq!(target, LinkTarget::Url("https://example.com/a".to_string()));
        assert_eq!(&text[range], "https://example.com/a");
    }

    #[test]
    fn detects_path_with_line_and_column() {
        let text = "at src/main.rs:12:3 ok";
        let (range, target) =
            detect_link(&Rope::from(text), offset_of(text, "main")).expect("link");
        assert_eq!(
            target,
            LinkTarget::Path {
                path: "src/main.rs".to_string(),
                line: Some(12),
                column: Some(3),
            }
        );
        assert_eq!(&text[range], "src/main.rs:12:3");
    }

    #[test]
    fn detects_bare_path_without_suffix() {
        let text = "open src/lib.rs now";
        let (range, target) = detect_link(&Rope::from(text), offset_of(text, "lib")).expect("link");
        assert_eq!(
            target,
            LinkTarget::Path {
                path: "src/lib.rs".to_string(),
                line: None,
                column: None,
            }
        );
        assert_eq!(&text[range], "src/lib.rs");
    }

    #[test]
    fn detects_path_with_line_only() {
        let text = "x dir/a.rs:9 y";
        let (_range, target) =
            detect_link(&Rope::from(text), offset_of(text, "a.rs")).expect("link");
        assert_eq!(
            target,
            LinkTarget::Path {
                path: "dir/a.rs".to_string(),
                line: Some(9),
                column: None,
            }
        );
    }

    #[test]
    fn rejects_token_without_path_shape() {
        let text = "hello world";
        assert_eq!(
            detect_link(&Rope::from(text), offset_of(text, "hello")),
            None
        );
    }

    #[test]
    fn rejects_bare_number_colon() {
        let text = "error 42:10 here";
        assert_eq!(detect_link(&Rope::from(text), offset_of(text, "42")), None);
    }

    #[test]
    fn returns_none_off_any_token() {
        assert_eq!(detect_link(&Rope::from("a b"), 1), None);
    }

    #[test]
    fn maps_offset_to_absolute_range_on_later_line() {
        let text = "first line\nsee https://x.io rest";
        let (range, target) =
            detect_link(&Rope::from(text), offset_of(text, "x.io")).expect("link");
        assert_eq!(target, LinkTarget::Url("https://x.io".to_string()));
        assert_eq!(&text[range.clone()], "https://x.io");
        assert!(range.start >= 11, "range must be absolute, got {range:?}");
    }
}
