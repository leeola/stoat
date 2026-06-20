use crate::ApcScene;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    symbols::border,
    widgets::StatefulWidget,
};
use stoatty_protocol::command::{self, BorderCommand, BorderStyle};

/// Frame the render area with a border.
///
/// Emits a `border` APC frame so a stoatty terminal draws crisp edges over the
/// area, and writes the matching box-drawing perimeter into `buf` so the same
/// frame degrades to a cell border in any other terminal. The border occupies
/// the area's perimeter cells; callers size the area to include that frame.
pub struct Border {
    pub style: BorderStyle,
    pub color: [u8; 3],
}

impl StatefulWidget for Border {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);

        command::encode_border_into(
            scene.buffer(),
            &BorderCommand {
                top: area.y,
                left: area.x,
                width: area.width,
                height: area.height,
                style: self.style,
                color: self.color,
            },
        );
    }
}

impl Border {
    fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let set = match self.style {
            BorderStyle::Light => border::PLAIN,
            BorderStyle::Heavy => border::THICK,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Rounded => border::ROUNDED,
        };
        let [r, g, b] = self.color;
        let style = Style::default().fg(Color::Rgb(r, g, b));

        let right = area.x + area.width - 1;
        let bottom = area.y + area.height - 1;

        put(buf, area.x, area.y, set.top_left, style);
        put(buf, right, area.y, set.top_right, style);
        put(buf, area.x, bottom, set.bottom_left, style);
        put(buf, right, bottom, set.bottom_right, style);

        for x in (area.x + 1)..right {
            put(buf, x, area.y, set.horizontal_top, style);
            put(buf, x, bottom, set.horizontal_bottom, style);
        }
        for y in (area.y + 1)..bottom {
            put(buf, area.x, y, set.vertical_left, style);
            put(buf, right, y, set.vertical_right, style);
        }
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::Border;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_border, BorderCommand, BorderStyle};

    fn symbol(buf: &Buffer, x: u16, y: u16) -> &str {
        buf.cell((x, y)).expect("cell in bounds").symbol()
    }

    #[test]
    fn emits_a_border_frame_over_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(2, 3, 10, 5);
        let mut buf = Buffer::empty(area);

        Border {
            style: BorderStyle::Rounded,
            color: [78, 86, 102],
        }
        .render(area, &mut buf, &mut scene);

        let expected = encode_border(&BorderCommand {
            top: 3,
            left: 2,
            width: 10,
            height: 5,
            style: BorderStyle::Rounded,
            color: [78, 86, 102],
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn draws_a_light_perimeter_fallback() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);

        Border {
            style: BorderStyle::Light,
            color: [255, 255, 255],
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "┌");
        assert_eq!(symbol(&buf, 2, 0), "┐");
        assert_eq!(symbol(&buf, 0, 2), "└");
        assert_eq!(symbol(&buf, 2, 2), "┘");
        assert_eq!(symbol(&buf, 1, 0), "─");
        assert_eq!(symbol(&buf, 0, 1), "│");
    }

    #[test]
    fn rounded_fallback_uses_arced_corners() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);

        Border {
            style: BorderStyle::Rounded,
            color: [1, 2, 3],
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "╭");
        assert_eq!(symbol(&buf, 2, 2), "╯");
    }
}
