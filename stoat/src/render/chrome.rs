use super::TEXT_SCALE_FULL;
use crate::{render::review::style_rgb, theme::Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, StatefulWidget, Widget},
};
use stoatty_protocol::command::{self, BorderStyle, PanelCommand};
use stoatty_widgets::{bar::Bar, text_run::TextRun, ApcScene};

/// Draw a modal frame around `area` and return the inner content rect.
///
/// This is the single chrome primitive behind every stoat modal and cursor
/// popup. The fallback -- taken when `scene` is absent or `style`'s foreground
/// is not RGB -- draws a ratatui [`Block`] with [`Borders::ALL`], `style` on the
/// border, and `title` styled the same, which is exactly what the sites drew
/// before, so their snapshots stay identical.
///
/// Under stoatty (a `scene` is threaded and the border color resolves to RGB) it
/// instead emits a hairline `panel` APC frame with rounded corners and a drop
/// shadow, plus, for `Some(title)`, a full-size title [`TextRun`] over the top
/// edge. The hairline runs unbroken through the title span, and the title run
/// carries no background box, so its glyphs blend directly over the grid cells
/// behind them. No box-drawing glyphs are written in this arm.
///
/// The caller owns background clearing, and a titled caller must clear the
/// border-row cells to the surface color so the title glyphs blend over a clean
/// surface rather than stale content. Sites that masked what was behind the
/// modal call [`Clear`](ratatui::widgets::Clear) before this; sites that paint
/// every cell themselves clear implicitly. This draws only the frame.
///
/// The returned rect is the area inset by the one-cell border, matching the
/// layout the sites lay their content out over regardless of arm.
pub(crate) fn modal_frame(
    buf: &mut Buffer,
    area: Rect,
    title: Option<&str>,
    style: Style,
    _theme: &Theme,
    scene: Option<&mut ApcScene>,
) -> Rect {
    let inner = Block::default().borders(Borders::ALL).inner(area);

    let rich = scene.and_then(|scene| {
        let border = style_rgb(style.fg)?;
        Some((scene, border))
    });

    match rich {
        Some((scene, border)) => {
            command::encode_panel_into(
                scene.buffer(),
                &PanelCommand {
                    top: area.y,
                    left: area.x,
                    width: area.width,
                    height: area.height,
                    style: BorderStyle::Rounded,
                    border,
                    corner_radius: 6,
                    fill: None,
                    shadow: true,
                },
            );
            if let Some(title) = title {
                TextRun {
                    col: 16,
                    row: 0,
                    scale: TEXT_SCALE_FULL,
                    color: border,
                    bg: None,
                    text: title,
                }
                .render(area, buf, scene);
            }
        },
        None => {
            let mut block = Block::default().borders(Borders::ALL).border_style(style);
            if let Some(title) = title {
                block = block.title(title.to_string()).title_style(style);
            }
            block.render(area, buf);
        },
    }

    inner
}

/// Draw a horizontal separator across `width` cells at row `y`, starting at
/// column `x`.
///
/// The fallback -- taken when `scene` is absent or `style`'s foreground is not
/// RGB -- writes `─` glyphs styled with `style`, exactly as the separator sites
/// did before. Under stoatty it emits one hairline [`Bar`] a sixteenth of a
/// cell thick centered in the row, and writes no glyphs.
pub(crate) fn hline(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
    scene: Option<&mut ApcScene>,
) {
    match scene.zip(style_rgb(style.fg)) {
        Some((scene, color)) => {
            Bar {
                x: 0,
                y: 8,
                width: width.saturating_mul(16),
                height: 1,
                color,
            }
            .render(Rect::new(x, y, width, 1), buf, scene);
        },
        None => {
            for col in x..x + width {
                buf[(col, y)].set_char('─').set_style(style);
            }
        },
    }
}

/// Draw a vertical separator down `height` cells at column `x`, starting at row
/// `y`.
///
/// The fallback -- taken when `scene` is absent or `style`'s foreground is not
/// RGB -- writes `│` glyphs styled with `style`, exactly as the separator sites
/// did before. Under stoatty it emits one hairline [`Bar`] a sixteenth of a
/// cell thick centered in the column, and writes no glyphs.
pub(crate) fn vline(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    height: u16,
    style: Style,
    scene: Option<&mut ApcScene>,
) {
    match scene.zip(style_rgb(style.fg)) {
        Some((scene, color)) => {
            Bar {
                x: 8,
                y: 0,
                width: 1,
                height: height.saturating_mul(16),
                color,
            }
            .render(Rect::new(x, y, 1, height), buf, scene);
        },
        None => {
            for row in y..y + height {
                buf[(x, row)].set_char('│').set_style(style);
            }
        },
    }
}

/// Draw `content` at cell `(x, y)`, clipped before column `end_x`.
///
/// The fallback -- taken when `scene` is absent, `style`'s foreground is not
/// RGB, or `bg` is `None` -- writes glyphs cell-by-cell styled with `style`,
/// stopping before `end_x`, exactly as the text sites did before. Under stoatty
/// it emits one [`TextRun`] at `scale` (256ths of a cell) anchored at the cell,
/// with `bg` as its background box and no grid glyphs.
///
/// `bg` is the run's own background. The renderer paints it as one opaque box
/// behind the alpha-blended glyphs, so it need not match the surface beneath.
#[allow(clippy::too_many_arguments)]
pub(crate) fn text(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    end_x: u16,
    content: &str,
    style: Style,
    bg: Option<[u8; 3]>,
    scale: u16,
    scene: Option<&mut ApcScene>,
) {
    match (scene, style_rgb(style.fg), bg) {
        (Some(scene), Some(color), Some(bg)) => {
            TextRun {
                col: 0,
                row: 0,
                scale,
                color,
                bg: Some(bg),
                text: content,
            }
            .render(Rect::new(x, y, 1, 1), buf, scene);
        },
        _ => {
            for (j, ch) in content.chars().enumerate() {
                let col = x + j as u16;
                if col >= end_x {
                    break;
                }
                buf[(col, y)].set_char(ch).set_style(style);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{hline, modal_frame, text, vline};
    use crate::theme::Theme;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };
    use stoatty_protocol::command::{
        encode_bar, encode_panel, encode_text_run, BarCommand, BorderStyle, PanelCommand,
        TextRunCommand,
    };
    use stoatty_widgets::ApcScene;

    fn rgb_style() -> Style {
        Style::default().fg(Color::Rgb(1, 2, 3))
    }

    #[test]
    fn fallback_draws_a_box_border_and_returns_the_inner_rect() {
        let area = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(area);
        let theme = Theme::empty();

        let inner = modal_frame(&mut buf, area, Some(" hi "), rgb_style(), &theme, None);

        assert_eq!(inner, Rect::new(1, 1, 6, 2));
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");
        assert_eq!(buf.cell((7, 3)).unwrap().symbol(), "┘");
        // The title glyphs land on the top border.
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "h");
    }

    #[test]
    fn stoatty_arm_emits_a_panel_and_no_border_glyphs() {
        let area = Rect::new(2, 1, 8, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut scene = ApcScene::new();
        let theme = Theme::empty();
        // The rich arm engages purely on the border color (style.fg) being RGB.
        let style = rgb_style();

        let inner = modal_frame(
            &mut buf,
            area,
            Some(" hi "),
            style,
            &theme,
            Some(&mut scene),
        );

        assert_eq!(inner, Rect::new(3, 2, 6, 2));
        // No box-drawing glyph is painted. The panel is off-grid.
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), " ");

        let panel = encode_panel(&PanelCommand {
            top: 1,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::Rounded,
            border: [1, 2, 3],
            corner_radius: 6,
            fill: None,
            shadow: true,
        });
        // The title run carries no background box and anchors one cell into the
        // modal (area.x * 16 + 16 = 48, area.y * 16 = 16), so the hairline draws
        // unbroken and the glyphs blend over the caller-cleared cells.
        let title = encode_text_run(&TextRunCommand {
            col: 48,
            row: 16,
            scale: 256,
            color: [1, 2, 3],
            bg: None,
            text: " hi ".to_owned(),
        });
        assert_eq!(scene.buffer(), &[panel, title].concat());
    }

    #[test]
    fn hline_fallback_draws_dashes_and_stoatty_emits_a_centered_bar() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 4));
        hline(&mut fallback, 2, 3, 4, rgb_style(), None);
        assert_eq!(fallback.cell((2, 3)).unwrap().symbol(), "─");
        assert_eq!(fallback.cell((5, 3)).unwrap().symbol(), "─");
        assert_eq!(fallback.cell((6, 3)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut scene = ApcScene::new();
        hline(&mut buf, 2, 3, 4, rgb_style(), Some(&mut scene));
        assert_eq!(buf.cell((2, 3)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_bar(&BarCommand {
                x: 32,
                y: 56,
                width: 64,
                height: 1,
                color: [1, 2, 3],
            })
        );
    }

    #[test]
    fn vline_fallback_draws_bars_and_stoatty_emits_a_centered_bar() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 4));
        vline(&mut fallback, 2, 1, 3, rgb_style(), None);
        assert_eq!(fallback.cell((2, 1)).unwrap().symbol(), "│");
        assert_eq!(fallback.cell((2, 3)).unwrap().symbol(), "│");
        assert_eq!(fallback.cell((2, 0)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut scene = ApcScene::new();
        vline(&mut buf, 2, 1, 3, rgb_style(), Some(&mut scene));
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_bar(&BarCommand {
                x: 40,
                y: 16,
                width: 1,
                height: 48,
                color: [1, 2, 3],
            })
        );
    }

    #[test]
    fn text_fallback_writes_clipped_glyphs_and_stoatty_emits_a_scaled_run() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 2));
        text(
            &mut fallback,
            1,
            0,
            5,
            "hello",
            rgb_style(),
            Some([9, 9, 9]),
            218,
            None,
        );
        assert_eq!(fallback.cell((1, 0)).unwrap().symbol(), "h");
        assert_eq!(fallback.cell((4, 0)).unwrap().symbol(), "l");
        // The 'o' would land on column 5, which is clipped at end_x.
        assert_eq!(fallback.cell((5, 0)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut scene = ApcScene::new();
        text(
            &mut buf,
            1,
            0,
            5,
            "hi",
            rgb_style(),
            Some([9, 9, 9]),
            218,
            Some(&mut scene),
        );
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_text_run(&TextRunCommand {
                col: 16,
                row: 0,
                scale: 218,
                color: [1, 2, 3],
                bg: Some([9, 9, 9]),
                text: "hi".to_owned(),
            })
        );
    }
}
