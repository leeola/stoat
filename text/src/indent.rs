use crate::Rope;

/// Largest space-indent width the detector distinguishes.
const MAX_INDENT: usize = 16;

/// Number of leading lines the detector scans before giving up.
const SCAN_LINES: u32 = 1000;

/// A buffer's indentation unit: hard tabs, or a fixed run of spaces.
///
/// `Spaces` holds the width in columns, valid for 1..=[`MAX_INDENT`]. The
/// [`Default`] is four spaces, the fallback when a file carries no detectable
/// indentation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum IndentStyle {
    Tabs,
    Spaces(u8),
}

impl Default for IndentStyle {
    fn default() -> Self {
        IndentStyle::Spaces(4)
    }
}

/// Detect the indentation style of `rope`, or `None` when the evidence is too
/// weak to be confident.
///
/// The style is voted on by histogramming the indentation *increase* between
/// consecutive non-blank lines over the first [`SCAN_LINES`] lines: tabs in one
/// bucket, each space-count increase in its own. Tabs weigh double (their
/// presence is a strong signal) and single-space increases weigh half (a single
/// leading space is more often prose than indentation). The winner is returned
/// only when it clearly beats the runner-up, so an unindented or empty file
/// yields `None` for the caller to resolve against a default.
pub fn detect_indent_style(rope: &Rope) -> Option<IndentStyle> {
    // Bucket 0 counts tab increases. Bucket n counts an n-space increase.
    let mut histogram = [0usize; MAX_INDENT + 1];
    let mut prev_is_tabs = false;
    let mut prev_count = 0usize;

    let last_row = rope.max_point().row;
    for row in 0..=last_row.min(SCAN_LINES.saturating_sub(1)) {
        let line = rope.line_at_row(row);
        let mut chars = line.chars();
        let is_tabs = match chars.next() {
            Some('\t') => true,
            Some(' ') => false,
            // A line starting with content ends the running indent comparison.
            Some(c) if !c.is_whitespace() => {
                prev_is_tabs = false;
                prev_count = 0;
                continue;
            },
            // A blank line (empty or leading line break) is skipped without
            // disturbing the running indent.
            _ => continue,
        };

        let mut count = 1usize;
        let mut counting = true;
        let mut has_content = false;
        for c in chars {
            match c {
                '\t' if is_tabs && counting => count += 1,
                ' ' if !is_tabs && counting => count += 1,
                c if c.is_whitespace() => counting = false,
                _ => {
                    has_content = true;
                    break;
                },
            }
            if count > 256 {
                break;
            }
        }
        if !has_content {
            continue;
        }

        // Record an indentation increase over the previous non-blank line.
        if (prev_is_tabs == is_tabs || prev_count == 0) && prev_count < count {
            if is_tabs {
                histogram[0] += 1;
            } else if count - prev_count <= MAX_INDENT {
                histogram[count - prev_count] += 1;
            }
        }
        prev_is_tabs = is_tabs;
        prev_count = count;
    }

    histogram[0] *= 2;
    if histogram[1] > 1 {
        histogram[1] /= 2;
    }

    let (indent, indent_freq) = histogram
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(_, freq)| freq)?;
    let runner_up = histogram
        .iter()
        .copied()
        .enumerate()
        .filter(|&(bucket, _)| bucket != indent)
        .map(|(_, freq)| freq)
        .max()
        .unwrap_or(0);

    if indent_freq >= 1 && (runner_up as f64 / indent_freq as f64) < 0.66 {
        Some(match indent {
            0 => IndentStyle::Tabs,
            width => IndentStyle::Spaces(width as u8),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_indent_style, IndentStyle};
    use crate::Rope;

    fn detect(text: &str) -> Option<IndentStyle> {
        detect_indent_style(&Rope::from(text))
    }

    #[test]
    fn tabs_detected() {
        assert_eq!(
            detect("fn a() {\n\tlet x = 1;\n\tif x {\n\t\tx;\n\t}\n}\n"),
            Some(IndentStyle::Tabs)
        );
    }

    #[test]
    fn two_spaces_detected() {
        assert_eq!(
            detect("fn a() {\n  let x = 1;\n  if x {\n    x;\n  }\n}\n"),
            Some(IndentStyle::Spaces(2))
        );
    }

    #[test]
    fn four_spaces_detected() {
        assert_eq!(
            detect("fn a() {\n    let x = 1;\n    if x {\n        x;\n    }\n}\n"),
            Some(IndentStyle::Spaces(4))
        );
    }

    #[test]
    fn unindented_is_none() {
        assert_eq!(detect("alpha\nbravo\ncharlie\n"), None);
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(detect(""), None);
    }

    #[test]
    fn default_is_four_spaces() {
        assert_eq!(IndentStyle::default(), IndentStyle::Spaces(4));
    }
}
