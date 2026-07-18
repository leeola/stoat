use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    symbols::border,
    widgets::StatefulWidget,
};
use stoatty_protocol::command::{self, BorderStyle, PanelCommand};

/// Frame the render area with off-grid modal chrome.
///
/// Emits a `panel` APC frame so a stoatty terminal draws a hairline frame with
/// rounded corners, an optional fill, and a drop shadow over the area. It also
/// writes the matching box-drawing perimeter into `buf`, so the same frame
/// degrades to a classic cell border in any other terminal. The frame occupies
/// the area's perimeter cells, so callers size the area to include it.
///
/// The fallback is frame-only. The [`Self::fill`] and [`Self::shadow`] are APC
/// details a plain terminal cannot draw, so it keeps the cells' own backgrounds.
pub struct Panel {
    pub style: BorderStyle,
    pub border: [u8; 3],
    pub corner_radius: u8,
    pub fill: Option<[u8; 3]>,
    pub shadow: bool,
    /// Device pixels shaved off each horizontal edge in the APC frame, so the box
    /// draws narrower than its cell rect. `0` is cell-exact. The fallback border
    /// ignores it (cell borders are whole cells).
    pub inset_x: u8,
}

impl StatefulWidget for Panel {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);

        command::encode_panel_into(
            scene.buffer(),
            &PanelCommand {
                top: area.y,
                left: area.x,
                width: area.width,
                height: area.height,
                style: self.style,
                border: self.border,
                corner_radius: self.corner_radius,
                fill: self.fill,
                shadow: self.shadow,
                inset_x: self.inset_x,
            },
        );
    }
}

impl Panel {
    fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let set = match self.style {
            BorderStyle::Light => border::PLAIN,
            BorderStyle::Heavy => border::THICK,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Rounded => border::ROUNDED,
        };
        let [r, g, b] = self.border;
        let style = Style::default().fg(Color::Rgb(r, g, b));

        cells::draw_perimeter(buf, area, set, style);
    }
}

#[cfg(test)]
mod tests {
    use super::Panel;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_panel, BorderStyle, PanelCommand};

    fn symbol(buf: &Buffer, x: u16, y: u16) -> &str {
        buf.cell((x, y)).expect("cell in bounds").symbol()
    }

    #[test]
    fn emits_a_panel_frame_over_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(2, 3, 10, 5);
        let mut buf = Buffer::empty(area);

        Panel {
            style: BorderStyle::Rounded,
            border: [78, 86, 102],
            corner_radius: 6,
            fill: Some([40, 44, 52]),
            shadow: true,
            inset_x: 4,
        }
        .render(area, &mut buf, &mut scene);

        let expected = encode_panel(&PanelCommand {
            top: 3,
            left: 2,
            width: 10,
            height: 5,
            style: BorderStyle::Rounded,
            border: [78, 86, 102],
            corner_radius: 6,
            fill: Some([40, 44, 52]),
            shadow: true,
            inset_x: 4,
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn draws_a_box_drawing_fallback() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);

        Panel {
            style: BorderStyle::Light,
            border: [255, 255, 255],
            corner_radius: 0,
            fill: None,
            shadow: false,
            inset_x: 0,
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

        Panel {
            style: BorderStyle::Rounded,
            border: [1, 2, 3],
            corner_radius: 6,
            fill: None,
            shadow: false,
            inset_x: 0,
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "╭");
        assert_eq!(symbol(&buf, 2, 2), "╯");
    }
}
