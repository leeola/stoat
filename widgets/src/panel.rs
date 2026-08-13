use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::StatefulWidget,
};
use stoatty_protocol::command::{self, BorderStyle, PanelCommand, PanelShadow};

/// Frame the render area with off-grid modal chrome.
///
/// Emits a `panel` APC frame so a stoatty terminal draws a hairline frame with
/// rounded corners, an optional fill, and a shadow over the area. It also
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
    pub shadow: PanelShadow,
    /// Device pixels shaved off each horizontal edge in the APC frame, so the box
    /// draws narrower than its cell rect. `0` is cell-exact. The fallback border
    /// ignores it (cell borders are whole cells).
    pub inset_x: u8,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it. The fallback border
    /// ignores it (a plain terminal has no pools).
    pub above_pools: bool,
}

impl StatefulWidget for Panel {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);
        self.draw_components(area, scene);
    }
}

impl Panel {
    /// Draw only the off-grid panel frame.
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded cell border
    /// and so doubles under the frame inside a rich terminal. Writes no cells.
    pub fn draw_components(&self, area: Rect, scene: &mut ApcScene) {
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
                above_pools: self.above_pools,
            },
        );
    }

    /// Draw only the degraded cell border, for a terminal without the off-grid
    /// frame.
    ///
    /// Frame-only. [`Self::fill`] and [`Self::shadow`] are APC details with no
    /// cell equivalent, so the cells keep their own backgrounds.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let set = match self.style {
            BorderStyle::Light => border::PLAIN,
            BorderStyle::Heavy => border::THICK,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Rounded => border::ROUNDED,
        };
        let style = Style::default().fg(cells::rgb(self.border));

        cells::draw_perimeter(buf, area, set, style);
    }
}

#[cfg(test)]
mod tests {
    use super::Panel;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_panel, BorderStyle, PanelCommand, PanelShadow};

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
            shadow: PanelShadow::Drop,
            inset_x: 4,
            above_pools: false,
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
            shadow: PanelShadow::Drop,
            inset_x: 4,
            above_pools: false,
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
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
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
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "╭");
        assert_eq!(symbol(&buf, 2, 2), "╯");
    }

    /// An app compositing its own chrome takes the halves apart. The frame half
    /// must leave the buffer alone, or the cell border shows through beneath
    /// the panel.
    #[test]
    fn the_two_halves_split_cleanly() {
        let area = Rect::new(0, 0, 3, 3);
        let panel = Panel {
            style: BorderStyle::Rounded,
            border: [1, 2, 3],
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
        };

        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(area);
        panel.draw_components(area, &mut scene);
        assert!(!scene.bytes().is_empty(), "the frame is emitted");
        assert_eq!(buf, Buffer::empty(area), "and no cell is written");

        let scene = ApcScene::new();
        panel.draw_fallback(area, &mut buf);
        assert_eq!(symbol(&buf, 0, 0), "╭", "the fallback writes cells");
        assert!(scene.bytes().is_empty(), "and emits nothing");
    }
}
