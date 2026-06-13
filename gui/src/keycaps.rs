//! Shared keycap-chip rendering: a chord display label drawn as a row of
//! small bordered chips, one per key. Used by the key-hint banner and the
//! command palette so keybindings look the same wherever they surface.

use gpui::{div, Div, Hsla, ParentElement, SharedString, Styled};

/// Render a chord display label (e.g. `"Spc p"`, `"z c"`, `":"`) as a row of
/// bordered keycap chips, one chip per space-separated key.
///
/// `text_color` styles the key glyphs and `border_color` the chip outline, so
/// callers pass whichever theme slots fit their surface. A single-key label
/// (e.g. a transient-mode binding) yields one chip.
pub(crate) fn chord(label: &str, text_color: Hsla, border_color: Hsla) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .children(label.split(' ').map(move |key| {
            div()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(border_color)
                .text_color(text_color)
                .text_xs()
                .child(SharedString::from(key.to_string()))
        }))
}
