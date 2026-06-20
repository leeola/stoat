use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    symbols::border,
    widgets::StatefulWidget,
};
use stoatty_protocol::command;

/// A floating popover box drawn above the grid.
///
/// Emits a `popover` APC frame so a stoatty terminal floats an opaque box with
/// its own z-order over the cells, and writes a bordered cell box with the
/// content clipped inside so the same frame degrades to a popover-shaped box in
/// any other terminal. The box fills the render area.
///
/// `content` is borrowed so a caller can pass a slice it already holds rather
/// than own a string per frame. `scale` is in 256ths of the cell size and
/// `offset` is a signed pixel nudge from the anchor; both shape only the rich
/// rendering, not the cell fallback.
pub struct Popover<'a> {
    pub fill: [u8; 3],
    pub border: [u8; 3],
    pub content_fg: [u8; 3],
    pub scale: u8,
    pub offset: [i16; 2],
    pub content: &'a str,
}

impl StatefulWidget for Popover<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);

        command::encode_popover_into(
            scene.buffer(),
            area.y,
            area.x,
            area.width,
            area.height,
            self.fill,
            self.border,
            self.content_fg,
            self.scale,
            self.offset,
            self.content,
        );
    }
}

impl Popover<'_> {
    fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let fill = rgb(self.fill);
        cells::fill(buf, area, Style::default().bg(fill));
        cells::draw_perimeter(
            buf,
            area,
            border::PLAIN,
            Style::default().fg(rgb(self.border)).bg(fill),
        );

        if area.width > 2 && area.height > 2 {
            buf.set_stringn(
                area.x + 1,
                area.y + 1,
                self.content,
                (area.width - 2) as usize,
                Style::default().fg(rgb(self.content_fg)).bg(fill),
            );
        }
    }
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::Popover;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_popover, PopoverCommand};

    fn symbol(buf: &Buffer, x: u16, y: u16) -> &str {
        buf.cell((x, y)).expect("cell in bounds").symbol()
    }

    #[test]
    fn emits_a_popover_frame_over_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(1, 1, 12, 4);
        let mut buf = Buffer::empty(area);

        Popover {
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [-3, 7],
            content: "hello",
        }
        .render(area, &mut buf, &mut scene);

        let expected = encode_popover(&PopoverCommand {
            top: 1,
            left: 1,
            width: 12,
            height: 4,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [-3, 7],
            content: "hello".to_owned(),
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn draws_a_bordered_box_with_clipped_content() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 6, 4);
        let mut buf = Buffer::empty(area);

        Popover {
            fill: [0, 0, 0],
            border: [255, 255, 255],
            content_fg: [255, 255, 255],
            scale: 1,
            offset: [0, 0],
            content: "abcdefgh",
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "┌");
        assert_eq!(symbol(&buf, 1, 1), "a");
        assert_eq!(symbol(&buf, 4, 1), "d");
        assert_eq!(symbol(&buf, 5, 1), "│");
        assert_eq!(symbol(&buf, 1, 2), " ");
    }
}
