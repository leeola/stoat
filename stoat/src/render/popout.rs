use crate::{
    render::{chrome, review, TEXT_SCALE_FULL},
    theme::{scope, Theme},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};
use stoatty_widgets::ApcScene;

/// Place a popout card of `height` rows above `status_area`.
///
/// The card is inset one cell on each side of `status_area`, centered, so it
/// reads as narrower than the bar it extends (see [`popout_inset`]). Its bottom
/// edge sits `stacked_above` rows above `status_area`'s top row, so a second
/// card clears one already placed at offset zero. Returns `None` when the card
/// would not fit within `content_area`, matching how a pane too short for a bar
/// segment simply drops it.
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

    let inset = popout_inset();
    Some(Rect::new(
        status_area.x + inset,
        top,
        status_area.width.saturating_sub(inset * 2),
        height,
    ))
}

/// Cells shaved off each horizontal side of a popout card, one per side, so the
/// card sits just inside the status bar's edges rather than flush with them.
///
/// The visible few-pixel gap where the editor background shows is carried by the
/// `POPOUT_INSET_PX` pixel inset (see [`crate::render::chrome`]); this one cell
/// keeps the card off the bar's exact edge columns.
pub(crate) fn popout_inset() -> u16 {
    1
}

/// The popout card's fill: the focused status-bar background nudged 35% toward
/// the editor background, so the card reads as a surface distinct from the bar
/// rather than blending into it.
///
/// Falls back to the bar background unchanged when either color is non-RGB (an
/// indexed-color theme), so those themes keep today's bar-flush look.
pub(crate) fn popout_card_bg(theme: &Theme) -> Color {
    let bar = theme.get(scope::UI_STATUSBAR_FOCUSED).bg;
    let editor = theme
        .try_get(scope::UI_BACKGROUND)
        .and_then(|style| style.bg);

    match (review::style_rgb(bar), review::style_rgb(editor)) {
        (Some(bar_rgb), Some(editor_rgb)) => {
            let [r, g, b] = review::dim_rgb(bar_rgb, editor_rgb, 0.35);
            Color::Rgb(r, g, b)
        },
        _ => bar.unwrap_or(Color::Reset),
    }
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

/// Greedy char-wrap `msg` into at most `max_rows` rows of `width` columns each.
///
/// Each row is filled to `width` characters before breaking, so a long word
/// splits across rows rather than pushing past the edge. When the message needs
/// more than `max_rows` rows, the last row ends with a `...` ellipsis to mark the
/// truncation. Returns an empty vector when `width` or `max_rows` is zero.
pub(crate) fn wrap_popout_lines(msg: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }

    let chars: Vec<char> = msg.chars().collect();
    let mut rows: Vec<String> = chars
        .chunks(width)
        .take(max_rows)
        .map(|chunk| chunk.iter().collect())
        .collect();

    if chars.len() > width * max_rows
        && let Some(last) = rows.last_mut()
    {
        let mut truncated: String = last.chars().take(width.saturating_sub(3)).collect();
        truncated.push_str("...");
        *last = truncated;
    }

    rows
}

/// The number of glyphs a row of `cells` cells fits at `scale`.
///
/// A glyph at `scale` (256ths of a cell) advances `scale`/[`TEXT_SCALE_FULL`] of a
/// cell, so a sub-cell scale fits more glyphs than the row has cells. Popout text
/// derives its wrap width and truncation from this so scaled lines fill the card
/// rather than stopping short at the cell count.
pub(crate) fn scaled_char_capacity(cells: usize, scale: u16) -> usize {
    cells * TEXT_SCALE_FULL as usize / scale as usize
}

#[cfg(test)]
mod tests {
    use super::{
        paint_popout_card, popout_area, popout_card_bg, popout_inset, scaled_char_capacity,
        wrap_popout_lines,
    };
    use crate::{render::review, theme::Theme};
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use stoatty_widgets::ApcScene;

    #[test]
    fn popout_area_sits_directly_above_the_bar_inset_one_cell() {
        let status = Rect::new(0, 20, 40, 1);
        let content = Rect::new(0, 0, 40, 20);

        // A 40-wide bar insets 1 cell each side, leaving a 38-wide card one row
        // above the bar.
        assert_eq!(
            popout_area(status, content, 1, 0),
            Some(Rect::new(1, 19, 38, 1))
        );
    }

    #[test]
    fn popout_inset_is_one_cell_per_side() {
        assert_eq!(popout_inset(), 1);
    }

    #[test]
    fn popout_area_centers_the_inset_card() {
        let status = Rect::new(4, 20, 100, 1);
        let content = Rect::new(0, 0, 120, 20);

        // 1-cell inset each side keeps the card centered under the bar.
        assert_eq!(
            popout_area(status, content, 1, 0),
            Some(Rect::new(5, 19, 98, 1))
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

    fn theme_from(src: &str) -> Theme {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Theme::from_config(&config.expect("parsed config"), "t").expect("theme load failed")
    }

    #[test]
    fn popout_card_bg_blends_the_bar_toward_the_editor() {
        let theme = theme_from(
            r##"theme t { ui.statusbar.focused.bg = "#283446"; ui.background.bg = "#181818"; }"##,
        );
        let [r, g, b] = review::dim_rgb([0x28, 0x34, 0x46], [0x18, 0x18, 0x18], 0.35);

        assert_ne!(
            popout_card_bg(&theme),
            Color::Rgb(0x28, 0x34, 0x46),
            "card fill differs from the bar"
        );
        assert_eq!(
            popout_card_bg(&theme),
            Color::Rgb(r, g, b),
            "card is the 0.35 blend"
        );
    }

    #[test]
    fn popout_card_bg_keeps_a_non_rgb_bar() {
        let theme = theme_from(
            r##"theme t { ui.statusbar.focused.bg = red; ui.background.bg = "#181818"; }"##,
        );

        assert_eq!(
            popout_card_bg(&theme),
            Color::Red,
            "a non-RGB bar keeps the bar background"
        );
    }

    #[test]
    fn scaled_char_capacity_grows_with_a_sub_cell_scale() {
        assert_eq!(
            scaled_char_capacity(10, 160),
            16,
            "160/256 fits 1.6x per cell"
        );
        assert_eq!(
            scaled_char_capacity(10, 256),
            10,
            "full-cell glyphs map 1:1"
        );
    }

    #[test]
    fn wrap_popout_lines_short_message_fits_one_row() {
        assert_eq!(wrap_popout_lines("oops", 10, 4), vec!["oops"]);
    }

    #[test]
    fn wrap_popout_lines_wraps_at_the_width() {
        assert_eq!(
            wrap_popout_lines("aaaaabbbbbccc", 5, 4),
            vec!["aaaaa", "bbbbb", "ccc"]
        );
    }

    #[test]
    fn wrap_popout_lines_caps_overflow_with_an_ellipsis() {
        assert_eq!(
            wrap_popout_lines("abcdefghijklm", 5, 2),
            vec!["abcde", "fg..."]
        );
    }

    #[test]
    fn wrap_popout_lines_zero_width_is_empty() {
        assert!(wrap_popout_lines("anything", 0, 4).is_empty());
    }
}
