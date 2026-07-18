use crate::{render::chrome, theme::Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};
use stoatty_widgets::ApcScene;

/// Place a popout card of `height` rows above `status_area`.
///
/// The returned rect spans `status_area`'s columns and its bottom edge sits
/// `stacked_above` rows above `status_area`'s top row, so a second card clears one
/// already placed at offset zero. Returns `None` when the card would not fit
/// within `content_area`, matching how a pane too short for a bar segment simply
/// drops it.
pub(crate) fn popout_area(
    status_area: Rect,
    content_area: Rect,
    height: u16,
    stacked_above: u16,
) -> Option<Rect> {
    let bottom = status_area.y.checked_sub(stacked_above)?;
    let top = bottom.checked_sub(height)?;

    if top < content_area.y || bottom > content_area.bottom() {
        return None;
    }

    Some(Rect::new(status_area.x, top, status_area.width, height))
}

/// Paint a popout card into `area` and return the rect its text draws into.
///
/// The leftmost and rightmost cell columns are glyph-cleared but keep their
/// existing style, so the editor background shows in the few-pixel strip outside
/// the inset frame rather than a squared background halo. The panel's own fill
/// covers the rest of those cells, and the interior columns are cleared to `bg`
/// spaces so the fallback and the pre-panel background pass both read `bg`.
///
/// The returned rect is `area` inset one cell on each horizontal side, where the
/// caller draws the card's text.
pub(crate) fn paint_popout_card(
    buf: &mut Buffer,
    area: Rect,
    bg: Color,
    border: Color,
    theme: &Theme,
    scene: Option<&mut ApcScene>,
) -> Rect {
    let right = area.x + area.width.saturating_sub(1);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if x == area.x || x == right {
                buf[(x, y)].set_char(' ');
            } else {
                buf[(x, y)].set_char(' ').set_style(Style::default().bg(bg));
            }
        }
    }

    chrome::popout_frame(buf, area, bg, border, theme, scene);

    Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    )
}

#[cfg(test)]
mod tests {
    use super::{paint_popout_card, popout_area};
    use crate::theme::Theme;
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use stoatty_widgets::ApcScene;

    #[test]
    fn popout_area_sits_directly_above_the_bar() {
        let status = Rect::new(0, 20, 40, 1);
        let content = Rect::new(0, 0, 40, 20);

        assert_eq!(
            popout_area(status, content, 1, 0),
            Some(Rect::new(0, 19, 40, 1))
        );
    }

    #[test]
    fn popout_area_stacks_one_row_higher_per_offset() {
        let status = Rect::new(0, 20, 40, 1);
        let content = Rect::new(0, 0, 40, 20);

        let base = popout_area(status, content, 1, 0).unwrap();
        let stacked = popout_area(status, content, 1, 1).unwrap();

        assert_eq!(stacked.y, base.y - 1);
    }

    #[test]
    fn popout_area_drops_when_content_is_too_short() {
        let status = Rect::new(0, 5, 40, 1);
        let content = Rect::new(0, 3, 40, 2);

        assert_eq!(popout_area(status, content, 3, 0), None);
    }

    #[test]
    fn paint_popout_card_keeps_edges_and_fills_interior() {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        let editor_bg = Color::Rgb(9, 9, 9);
        for y in 0..2 {
            buf[(0, y)].bg = editor_bg;
            buf[(5, y)].bg = editor_bg;
        }
        let bg = Color::Rgb(40, 44, 52);
        let border = Color::Rgb(78, 86, 102);
        let theme = Theme::empty();
        let mut scene = ApcScene::new();

        let content = paint_popout_card(&mut buf, area, bg, border, &theme, Some(&mut scene));

        assert_eq!(content, Rect::new(1, 0, 4, 2));
        assert_eq!(buf[(0, 0)].bg, editor_bg, "left edge keeps editor bg");
        assert_eq!(buf[(5, 1)].bg, editor_bg, "right edge keeps editor bg");
        assert_eq!(buf[(0, 0)].symbol(), " ", "edge glyph cleared");
        assert_eq!(buf[(2, 0)].bg, bg, "interior filled with card bg");
        assert_eq!(buf[(3, 1)].bg, bg, "interior filled with card bg");
    }
}
