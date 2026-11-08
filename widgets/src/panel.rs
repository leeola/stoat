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
    /// Pixels shaved off the top and bottom edges in the APC frame, as
    /// [`Self::inset_x`] shaves the sides. The fallback border ignores it.
    pub inset_y: u8,
    /// The corners [`Self::corner_radius`] rounds in the APC frame, as
    /// [`PanelCommand::corners`] names them. Zero rounds all four. The fallback
    /// border ignores it.
    pub corners: u8,
    /// The bottom corners the APC frame joins to the surface below with a
    /// fillet, as [`PanelCommand::fillets`] names them. The fallback border
    /// ignores it.
    pub fillets: u8,
    /// The color of the hatch over the APC frame's fill, or `None` for a plain
    /// fill. The fallback border ignores it.
    pub hatch: Option<[u8; 3]>,
    /// The panel floats above every pooled surface, so pool composites must not
    /// paint over its rect. `false` layers the panel with the grid, where a pool
    /// composite covering the same cells draws over it. The fallback border
    /// ignores it (a plain terminal has no pools).
    pub above_pools: bool,
    /// The panel sits under everything it overlaps, so no pool composite and no
    /// decoration takes its rect as an occluder, as a pane's island halo needs.
    /// A terminal before
    /// [`BACKDROP_PANEL_PROTOCOL`](stoatty_protocol::BACKDROP_PANEL_PROTOCOL)
    /// reads it as [`Self::above_pools`]. The fallback border ignores it.
    pub backdrop: bool,
    /// The host pool this panel rides and the document top row its layout
    /// assumed, or `None` for a panel fixed to the screen.
    ///
    /// `Some` makes the frame travel with a gliding pane beneath it instead of
    /// holding its screen position. The fallback border ignores it (a plain
    /// terminal has no pools, and text and frame land together at the repaint).
    pub anchor: Option<(u32, f32)>,
}

impl StatefulWidget for Panel {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);
        self.draw_components(area, scene);
    }
}

impl Panel {
    /// A chip's box in `fill`, one row of cells that holds a short label.
    ///
    /// The box stands one logical pixel inside the row's top and bottom edges,
    /// with corners of radius 1, so a chip reads as a box inside its row and
    /// not as a band across the row. It draws no line and no shadow. The cells
    /// under it keep the row's background, and their glyphs draw over the
    /// fill.
    pub fn chip(fill: [u8; 3]) -> Panel {
        Panel {
            style: BorderStyle::None_,
            border: fill,
            corner_radius: 1,
            fill: Some(fill),
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 1,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        }
    }

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
                inset_y: self.inset_y,
                corners: self.corners,
                fillets: self.fillets,
                hatch: self.hatch,
                above_pools: self.above_pools,
                backdrop: self.backdrop,
                anchor: self.anchor,
            },
        );
    }

    /// Draw only the degraded cell border, for a terminal without the off-grid
    /// frame.
    ///
    /// Frame-only. [`Self::fill`] and [`Self::shadow`] are APC details with no
    /// cell equivalent, so the cells keep their own backgrounds.
    /// [`BorderStyle::None_`] draws no line, so it writes no cells.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let set = match self.style {
            BorderStyle::Light => border::PLAIN,
            BorderStyle::Heavy => border::THICK,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Rounded => border::ROUNDED,
            BorderStyle::Dashed => cells::DASHED,
            BorderStyle::None_ => return,
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
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
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
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
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
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
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
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        }
        .render(area, &mut buf, &mut scene);

        assert_eq!(symbol(&buf, 0, 0), "╭");
        assert_eq!(symbol(&buf, 2, 2), "╯");
    }

    /// A dashed frame degrades to the light corners with dashed lines between
    /// them.
    #[test]
    fn dashed_fallback_draws_dashed_lines_between_light_corners() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);

        Panel {
            style: BorderStyle::Dashed,
            border: [1, 2, 3],
            corner_radius: 2,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        }
        .render(area, &mut buf, &mut scene);

        let row = |y| (0..3).map(|x| symbol(&buf, x, y)).collect::<String>();
        assert_eq!(
            [row(0), row(1), row(2)],
            [
                "\u{250c}\u{254c}\u{2510}",
                "\u{254e} \u{254e}",
                "\u{2514}\u{254c}\u{2518}"
            ],
        );
    }

    /// A panel in the style that draws no line has no cell border to degrade
    /// to, so its fallback leaves the buffer alone.
    #[test]
    fn the_fallback_with_no_line_writes_no_cell() {
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);

        Panel {
            style: BorderStyle::None_,
            border: [1, 2, 3],
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        }
        .draw_fallback(area, &mut buf);

        assert_eq!(buf, Buffer::empty(area));
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
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
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
