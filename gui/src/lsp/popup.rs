use crate::theme::ThemeColors;
use gpui::{div, point, Bounds, Div, Pixels, Point, Size, Styled};

/// Pixel origin one row below the cell at grid `(row, col)`, anchored to
/// the editor's `text_region_bounds`. Shared by the hover and
/// signature-help popups.
pub fn popup_origin_below(
    bounds: Bounds<Pixels>,
    cell: Size<Pixels>,
    row: u32,
    col: u32,
) -> Point<Pixels> {
    let x = bounds.origin.x + cell.width * col as f32;
    let y = bounds.origin.y + cell.height * (row + 1) as f32;
    point(x, y)
}

/// Absolutely-positioned popup chrome (padding, background, border) used
/// by the floating LSP popups. Callers add their own content via
/// [`gpui::ParentElement::child`] and wrap the result in
/// [`gpui::deferred`].
pub fn popup_container(origin: Point<Pixels>, theme: &ThemeColors) -> Div {
    div()
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .px_2()
        .py_1()
        .bg(theme.popup_background)
        .text_color(theme.popup_text)
        .border_1()
        .border_color(theme.popup_border)
}
