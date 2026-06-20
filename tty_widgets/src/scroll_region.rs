use crate::ApcScene;
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, ScrollRegionCommand};

/// Declare the render area as a scrollable sub-rectangle of the grid.
///
/// Emits a `scroll_region` APC frame reporting the region's current scroll
/// position in rows; a stoatty terminal eases the region's content as the offset
/// changes between frames, so the program reports an absolute position and the
/// terminal owns the animation. There is no cell fallback: in any other terminal
/// the content scrolls ordinarily, which the frame degrades to.
pub struct ScrollRegion {
    /// Current scroll position of the region, in rows.
    pub offset: u16,
}

impl StatefulWidget for ScrollRegion {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        command::encode_scroll_region_into(
            scene.buffer(),
            &ScrollRegionCommand {
                top: area.y,
                left: area.x,
                width: area.width,
                height: area.height,
                offset: self.offset,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::ScrollRegion;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_scroll_region, ScrollRegionCommand};

    #[test]
    fn emits_a_scroll_region_frame_over_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(2, 1, 40, 20);
        let mut buf = Buffer::empty(area);

        ScrollRegion { offset: 5 }.render(area, &mut buf, &mut scene);

        let expected = encode_scroll_region(&ScrollRegionCommand {
            top: 1,
            left: 2,
            width: 40,
            height: 20,
            offset: 5,
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn writes_no_cell_fallback() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 4, 3);
        let mut buf = Buffer::empty(area);

        ScrollRegion { offset: 2 }.render(area, &mut buf, &mut scene);

        assert_eq!(
            buf,
            Buffer::empty(area),
            "ScrollRegion degrades to ordinary scrolling, writing no cells"
        );
    }
}
