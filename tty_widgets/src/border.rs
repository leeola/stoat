use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::StatefulWidget,
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
        self.draw_components(area, scene);
    }
}

impl Border {
    /// Draw only the off-grid border frame.
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded cell border
    /// and so doubles under the frame inside a rich terminal. Writes no cells.
    pub fn draw_components(&self, area: Rect, scene: &mut ApcScene) {
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

    /// Draw only the degraded cell perimeter, for a terminal without the
    /// off-grid frame.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let set = match self.style {
            BorderStyle::Light => border::PLAIN,
            BorderStyle::Heavy => border::THICK,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Rounded => border::ROUNDED,
        };
        let style = Style::default().fg(cells::rgb(self.color));

        cells::draw_perimeter(buf, area, set, style);
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

    /// An app compositing its own chrome takes the halves apart. The frame half
    /// must leave the buffer alone, or the cell border shows through beneath it.
    #[test]
    fn the_two_halves_split_cleanly() {
        let area = Rect::new(0, 0, 3, 3);
        let border = Border {
            style: BorderStyle::Light,
            color: [1, 2, 3],
        };

        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(area);
        border.draw_components(area, &mut scene);
        assert!(!scene.bytes().is_empty(), "the frame is emitted");
        assert_eq!(buf, Buffer::empty(area), "and no cell is written");

        let scene = ApcScene::new();
        border.draw_fallback(area, &mut buf);
        assert_eq!(symbol(&buf, 0, 0), "┌", "the fallback writes cells");
        assert!(scene.bytes().is_empty(), "and emits nothing");
    }
}
