use crate::ApcScene;
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, ScaleCommand};

/// Draw the glyph at the render area's top-left cell scaled up.
///
/// Emits a `scale` APC frame so a stoatty terminal draws the cell's glyph over a
/// `scale` by `scale` block, claiming the rest of the block so neighbors do not
/// draw into it. The glyph is whatever the VT stream already wrote at the cell, so
/// the cell fallback is that base glyph at its normal size and the widget writes
/// nothing of its own; in any other terminal the text simply renders unscaled.
pub struct Scale {
    /// Integer multiple of the cell size the glyph is drawn at.
    pub scale: u8,
}

impl StatefulWidget for Scale {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        command::encode_scale_into(
            scene.buffer(),
            &ScaleCommand {
                top: area.y,
                left: area.x,
                scale: self.scale,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Scale;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_scale, ScaleCommand};

    #[test]
    fn emits_a_scale_frame_at_the_area_origin() {
        let mut scene = ApcScene::new();
        let area = Rect::new(4, 6, 2, 2);
        let mut buf = Buffer::empty(area);

        Scale { scale: 2 }.render(area, &mut buf, &mut scene);

        let expected = encode_scale(&ScaleCommand {
            top: 6,
            left: 4,
            scale: 2,
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn writes_no_cell_fallback() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);

        Scale { scale: 2 }.render(area, &mut buf, &mut scene);

        assert_eq!(
            buf,
            Buffer::empty(area),
            "Scale leaves the base glyph in place, writing no cells"
        );
    }
}
