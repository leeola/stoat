//! The checkout banner, as the BlameCheckout board draws it.
//!
//! A blame checkout detaches HEAD at the commit of a blamed line, and every
//! pane then shows that commit's tree. The banner keeps the checkout in view
//! under the tab bar until the return, and names the key that returns.

use crate::{
    app::BANNER_PAD,
    render::{self, paint::style_rgb, text, TypeScale},
    review_walk::{ReturnRef, ReviewWalk},
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use stoat_widgets::{
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// The logical px the banner draws below its row while the tab bar shows.
///
/// The banner's box then ends 8 px above the island under it, the gap the
/// board leaves under the banner. With the tab bar hidden, the banner draws
/// [`BANNER_PAD`] px down instead, so its box starts at the window's top.
const UNDER_BAR_NUDGE: i16 = 2;

/// The key the banner names the return by, the default chord that returns
/// while a walk is open.
const RETURN_KEY: &str = "Space h c";

/// The texts of a walk's banner, each with the style it draws in.
///
/// The commit's pair leads the row, and the return's pair ends it.
struct BannerTexts {
    commit: [(String, Style); 2],
    return_to: [(String, Style); 2],
}

impl BannerTexts {
    /// The texts of `walk`'s banner, in `banner` and the text colors of
    /// `theme`.
    ///
    /// The return names the branch the walk returns to, or the short sha of a
    /// walk that started detached.
    fn of(walk: &ReviewWalk, banner: Style, theme: &Theme) -> BannerTexts {
        let tinted = |name: &str| match theme.get(name).fg {
            Some(fg) => banner.fg(fg),
            None => banner,
        };
        let target = match &walk.return_ref {
            ReturnRef::Branch(name) => name.as_str(),
            ReturnRef::Detached(sha) => &sha[..sha.len().min(7)],
        };
        let origin = match &walk.stash {
            Some(stash) if stash.changes == 1 => {
                format!("checked out from blame \u{b7} 1 change stashed from {target}")
            },
            Some(stash) => format!(
                "checked out from blame \u{b7} {} changes stashed from {target}",
                stash.changes
            ),
            None => "checked out from blame".to_owned(),
        };

        BannerTexts {
            commit: [
                (
                    format!("detached at {}", walk.current().short_sha),
                    banner.add_modifier(Modifier::BOLD),
                ),
                (origin, tinted(scope::UI_TEXT_DIM)),
            ],
            return_to: [
                (RETURN_KEY.to_owned(), tinted(scope::UI_KEY_LABEL)),
                (format!(" return to {target}"), tinted(scope::UI_TEXT_MUTED)),
            ],
        }
    }
}

/// Paint the checkout banner of `walk` across `row`, the cell row under the
/// tab bar.
///
/// Under `bands` and a live scene, the banner is a panel in its background on
/// the window ground, and its texts draw on the panel as runs. The panel stands
/// [`BANNER_PAD`] px outside `row` at the top and the bottom and 1 px outside
/// it at each side. The banner draws [`UNDER_BAR_NUDGE`] px down while
/// `tab_bar` shows, and [`BANNER_PAD`] px down without it. Otherwise the banner
/// is the cells of `row` in its background.
///
/// The commit's texts start a cell in and clip a cell short of the row's end.
/// The return's key and branch end a cell short of the row's end, and they
/// drop where the commit's texts leave them no room.
pub(crate) fn render_checkout_banner(
    walk: &ReviewWalk,
    row: Rect,
    bands: bool,
    tab_bar: bool,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let banner = banner_style(theme);
    let texts = BannerTexts::of(walk, banner, theme);
    let Some(fill) = style_rgb(banner.bg).filter(|_| bands && scene.live()) else {
        paint_cells(&texts, row, banner, buf);
        return;
    };

    let nudge = match tab_bar {
        true => UNDER_BAR_NUDGE,
        false => i16::from(BANNER_PAD),
    };
    scene.nudge([0, nudge]);
    paint_band(&texts, row, fill, theme, buf, scene);
    scene.nudge([0, 0]);
}

/// The banner's style, from `vcs.checkout.banner`.
///
/// A theme that leaves the scope's colors unset draws the banner in the active
/// badge's foreground on the window ground.
fn banner_style(theme: &Theme) -> Style {
    let banner = theme
        .try_get(scope::VCS_CHECKOUT_BANNER)
        .unwrap_or_default();
    Style {
        fg: banner.fg.or_else(|| theme.get(scope::UI_BADGE_ACTIVE).fg),
        bg: banner.bg.or_else(|| render::ground_surface(theme)),
        ..banner
    }
}

/// Write `texts` as the cells of `row`, on the banner's background.
fn paint_cells(texts: &BannerTexts, row: Rect, banner: Style, buf: &mut Buffer) {
    for x in row.left()..row.right() {
        buf[(x, row.y)].set_char(' ').set_style(banner);
    }

    let last = row.right().saturating_sub(1);
    let mut x = row.x + 1;
    for (part, style) in &texts.commit {
        text::write_str_clipped(buf, x, row.y, part, *style, last);
        x = x.saturating_add(part.chars().count() as u16 + 1);
    }

    let width: usize = texts
        .return_to
        .iter()
        .map(|(part, _)| part.chars().count())
        .sum();
    let Some(mut x) = last.checked_sub(width as u16).filter(|&start| start >= x) else {
        return;
    };
    for (part, style) in &texts.return_to {
        text::write_str(buf, x, row.y, part, *style);
        x += part.chars().count() as u16;
    }
}

/// Draw the banner's panel in `fill` around `row`, and `texts` on it as runs,
/// the commit's at the Ui scale and the return's at the Caption scale.
fn paint_band(
    texts: &BannerTexts,
    row: Rect,
    fill: [u8; 3],
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    Panel {
        style: BorderStyle::None_,
        border: fill,
        corner_radius: 6,
        fill: Some(fill),
        shadow: PanelShadow::None_,
        edges: [
            Edge::Outset(BANNER_PAD),
            Edge::Outset(1),
            Edge::Outset(BANNER_PAD),
            Edge::Outset(1),
        ],
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
    }
    .draw_components(row, scene);

    let mut run = |part: &str, style: Style, col: u16, scale: u16| {
        let Some(color) = style_rgb(style.fg) else {
            return;
        };
        TextRun {
            col: col as i16,
            row: 0,
            scale,
            color,
            bg: Some(fill),
            bold: style.add_modifier.contains(Modifier::BOLD),
            italic: false,
            text: part,
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(row.x, row.y, 1, 1), buf, scene);
    };

    let end = row.width.saturating_sub(1).saturating_mul(16);
    let ui = theme.type_scale(TypeRole::Ui);
    let mut col: u16 = 16;
    for (part, style) in &texts.commit {
        let part = fit(part, end.saturating_sub(col), ui);
        run(part, *style, col, ui);
        col = col.saturating_add(text_run::advance_sixteenths(part.chars().count(), ui) + 16);
    }

    let caption = theme.type_scale(TypeRole::Caption);
    let advance = |part: &str| text_run::advance_sixteenths(part.chars().count(), caption);
    let width: u16 = texts.return_to.iter().map(|(part, _)| advance(part)).sum();
    let Some(mut col) = end.checked_sub(width).filter(|&start| start >= col) else {
        return;
    };
    for (part, style) in &texts.return_to {
        run(part, *style, col, caption);
        col += advance(part);
    }
}

/// The leading part of `text` whose run at `scale` advances no further than
/// `room` sixteenths.
fn fit(text: &str, room: u16, scale: u16) -> &str {
    let fits = usize::from(room) * 16 / usize::from(scale.max(1));
    match text.char_indices().nth(fits) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::banner_style;
    use crate::theme::Theme;
    use ratatui::style::Color;

    /// A theme that leaves the banner's scope unset draws the banner in the
    /// active badge's foreground on the window ground.
    #[test]
    fn a_theme_without_the_banner_scope_takes_the_badge_color_on_the_ground() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.badge.active.fg = "#e8bd6a";
                ui.surface.ground.bg = "#0c0e11";
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let style = banner_style(&theme);
        assert_eq!(
            (style.fg, style.bg),
            (
                Some(Color::Rgb(0xe8, 0xbd, 0x6a)),
                Some(Color::Rgb(0x0c, 0x0e, 0x11)),
            ),
        );
    }
}
