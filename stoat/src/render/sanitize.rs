//! Shared input-sanitization helpers for surfaces that paint
//! untrusted bytes into a ratatui buffer.
//!
//! The preview pane in the file finder, the (forthcoming) git diff
//! preview, log and output panes, additional pickers, and in-pane
//! terminals all read bytes that may carry CSI escape sequences,
//! BEL, CR, or other C0 controls that the host terminal would
//! interpret. Routing every such surface through one helper keeps
//! the contract -- "what's safe to paint" -- in a single place.

/// Replace C0 control characters (other than `\n` and `\t`) and
/// DEL with `·`. The preview pane writes its content into a
/// ratatui buffer one cell per char; an unfiltered ESC starts a
/// real CSI sequence in the host terminal, BEL beeps, CR jumps
/// the cursor to column 0. The editor's display map expands `\t`
/// and the renderer treats `\n` as a row break, so those two
/// characters pass through unchanged.
pub(crate) fn sanitize_preview_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push('·'),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_preview_text_passes_plain_ascii_through() {
        assert_eq!(sanitize_preview_text("hello world"), "hello world");
    }

    #[test]
    fn sanitize_preview_text_keeps_newline_and_tab() {
        assert_eq!(sanitize_preview_text("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn sanitize_preview_text_redacts_esc_csi_sequence() {
        assert_eq!(sanitize_preview_text("\x1b[31mhi\x1b[0m"), "·[31mhi·[0m",);
    }

    #[test]
    fn sanitize_preview_text_redacts_cr_bel_nul_del() {
        assert_eq!(sanitize_preview_text("\r\x07\x00\x7f"), "····");
    }

    #[test]
    fn sanitize_preview_text_passes_multibyte_utf8_through() {
        assert_eq!(sanitize_preview_text("café naïve"), "café naïve");
    }
}
