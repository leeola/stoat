use crate::{
    render::review::style_rgb,
    theme::{scope, Theme},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, StatefulWidget, Widget},
};
use stoatty_protocol::command::{self, BorderStyle, PanelCommand};
use stoatty_widgets::{text_run::TextRun, ApcScene};

/// Draw a modal frame around `area` and return the inner content rect.
///
/// This is the single chrome primitive behind every stoat modal and cursor
/// popup. The fallback -- taken when `scene` is absent or the theme is not an
/// RGB theme -- clears `area` and draws a ratatui [`Block`] with
/// [`Borders::ALL`], `style` on the border, and `title` styled the same, which
/// is exactly what the sites drew before, so their snapshots stay identical.
///
/// Under stoatty (a `scene` is threaded and the colors resolve to RGB) it
/// instead emits a hairline `panel` APC frame with rounded corners and a drop
/// shadow, plus, for `Some(title)`, a full-size title [`TextRun`] over the top
/// edge whose background masks the hairline the way a Block title masks the
/// border glyphs. No box-drawing glyphs are written in this arm.
///
/// The returned rect is the area inset by the one-cell border, matching the
/// layout the sites lay their content out over regardless of arm.
pub(crate) fn modal_frame(
    buf: &mut Buffer,
    area: Rect,
    title: Option<&str>,
    style: Style,
    theme: &Theme,
    scene: Option<&mut ApcScene>,
) -> Rect {
    let inner = Block::default().borders(Borders::ALL).inner(area);

    let rich = scene.and_then(|scene| {
        let border = style_rgb(style.fg)?;
        let mask = style_rgb(
            style
                .bg
                .or_else(|| theme.try_get(scope::UI_BACKGROUND).and_then(|s| s.bg)),
        )?;
        Some((scene, border, mask))
    });

    match rich {
        Some((scene, border, mask)) => {
            Clear.render(area, buf);
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
                    scale: 256,
                    color: border,
                    bg: mask,
                    text: title,
                }
                .render(area, buf, scene);
            }
        },
        None => {
            Clear.render(area, buf);
            let mut block = Block::default().borders(Borders::ALL).border_style(style);
            if let Some(title) = title {
                block = block.title(title.to_string()).title_style(style);
            }
            block.render(area, buf);
        },
    }

    inner
}

#[cfg(test)]
mod tests {
    use super::modal_frame;
    use crate::theme::Theme;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };
    use stoatty_protocol::command::{encode_panel, BorderStyle, PanelCommand};
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
        // A bg on the style makes the title mask resolve without a themed
        // background, so the rich arm engages.
        let style = rgb_style().bg(Color::Rgb(9, 9, 9));

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
        assert!(
            scene.buffer().starts_with(&panel),
            "the panel frame at the modal rect leads the batch",
        );
        assert!(
            scene.buffer().len() > panel.len(),
            "the title text run follows the panel",
        );
    }
}
