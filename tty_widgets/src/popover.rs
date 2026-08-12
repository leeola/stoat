use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    symbols::border,
    widgets::StatefulWidget,
};
use stoatty_protocol::command::{self, PopoverCommand};

/// A floating popover box drawn above the grid.
///
/// Emits a `popover` APC frame so a stoatty terminal floats an opaque box with
/// its own z-order over the cells, and writes a bordered cell box with the
/// content clipped inside so the same frame degrades to a popover-shaped box in
/// any other terminal. The box fills the render area.
///
/// `content` is borrowed so a caller can pass a slice it already holds rather
/// than own a string per frame. `scale` is an integer multiple of the cell size
/// (1 draws one cell per glyph, 2 a 2x2 block) and `offset` is a signed pixel
/// nudge from the anchor. Both shape only the rich rendering, not the cell
/// fallback. Rich content is inset one cell from the border, matching the cell
/// fallback's inset.
///
/// `bold` shapes the content at bold weight in both the rich rendering and the
/// cell fallback.
pub struct Popover<'a> {
    pub fill: [u8; 3],
    pub border: [u8; 3],
    pub content_fg: [u8; 3],
    pub scale: u8,
    pub offset: [i16; 2],
    pub bold: bool,
    pub content: &'a str,
}

impl StatefulWidget for Popover<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);
        self.draw_components(area, scene);
    }
}

impl Popover<'_> {
    /// Draw only the off-grid popover.
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded cell box
    /// and so doubles under the popover inside a rich terminal. Writes no
    /// cells.
    ///
    /// The content rides outside the APC wrapper, so a host that is not stoatty
    /// prints it as characters. Emit this only into a live scene.
    pub fn draw_components(&self, area: Rect, scene: &mut ApcScene) {
        command::encode_popover_into(
            scene.buffer(),
            &PopoverCommand {
                top: area.y,
                left: area.x,
                width: area.width,
                height: area.height,
                fill: self.fill,
                border: self.border,
                content_fg: self.content_fg,
                scale: self.scale,
                offset: self.offset,
                bold: self.bold,
                content: self.content,
            },
        );
    }

    /// Draw only the degraded cell box and its content, for a terminal without
    /// the off-grid popover.
    ///
    /// Each line of `content` takes one row. The box never grows to fit, so
    /// lines past its height and text past its width are dropped. A caller
    /// sizes the area from its own content to avoid that.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let fill = rgb(self.fill);
        cells::fill(buf, area, Style::default().bg(fill));
        cells::draw_perimeter(
            buf,
            area,
            border::PLAIN,
            Style::default().fg(rgb(self.border)).bg(fill),
        );

        if area.width > 2 && area.height > 2 {
            let mut content_style = Style::default().fg(rgb(self.content_fg)).bg(fill);
            if self.bold {
                content_style = content_style.add_modifier(Modifier::BOLD);
            }
            let width = (area.width - 2) as usize;
            let rows = self.content.lines().take((area.height - 2) as usize);
            for (row, line) in rows.enumerate() {
                let y = area.y + 1 + row as u16;
                buf.set_stringn(area.x + 1, y, line, width, content_style);
            }
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

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| symbol(buf, x, y))
            .collect()
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
            bold: true,
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
            bold: true,
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
            bold: false,
            content: "abcdefgh",
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "┌");
        assert_eq!(symbol(&buf, 1, 1), "a");
        assert_eq!(symbol(&buf, 4, 1), "d");
        assert_eq!(symbol(&buf, 5, 1), "│");
        assert_eq!(symbol(&buf, 1, 2), " ");
    }

    /// Every multi-line caller joins its lines with newlines, which ratatui
    /// drops as control characters if the box writes them as one string.
    #[test]
    fn each_content_line_takes_its_own_row() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 6, 4);
        let mut buf = Buffer::empty(area);

        Popover {
            fill: [0, 0, 0],
            border: [255, 255, 255],
            content_fg: [255, 255, 255],
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "one\ntwo\nthree",
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(
            row(&buf, 1),
            "│one │",
            "the first line sits on the first row"
        );
        assert_eq!(row(&buf, 2), "│two │", "and the second on the next");
        assert_eq!(
            row(&buf, 3),
            "└────┘",
            "the third line does not fit and never overwrites the border"
        );
    }

    /// An app compositing its own chrome takes the halves apart. The frame half
    /// must leave the buffer alone, or the cell box shows through beneath the
    /// popover.
    #[test]
    fn the_two_halves_split_cleanly() {
        let area = Rect::new(0, 0, 6, 3);
        let popover = Popover {
            fill: [1, 2, 3],
            border: [4, 5, 6],
            content_fg: [7, 8, 9],
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "abc",
        };

        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(area);
        popover.draw_components(area, &mut scene);
        assert!(!scene.bytes().is_empty(), "the popover is emitted");
        assert_eq!(buf, Buffer::empty(area), "and no cell is written");

        let scene = ApcScene::new();
        popover.draw_fallback(area, &mut buf);
        assert_eq!(symbol(&buf, 1, 1), "a", "the fallback writes cells");
        assert!(scene.bytes().is_empty(), "and emits nothing");
    }
}
