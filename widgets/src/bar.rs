use crate::{cells, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, BarCommand};

/// A solid sub-cell rectangle drawn off the cell grid.
///
/// A gutter packs several variable-width status or git bars and a hairline
/// separator into a fraction of a cell; `Bar` is one such fill. All four
/// coordinates are in **sixteenths of a cell** (16 = one cell) relative to the
/// render area's top-left, so a bar can be a fraction of a cell wide and track
/// live font zoom. There is no cell fallback: a bar is inherently sub-cell.
pub struct Bar {
    /// Left edge in sixteenths, along the cell width, from the area's left.
    /// A negative value overhangs the area to the left.
    pub x: i16,
    /// Top edge in sixteenths, along the cell height, from the area's top.
    /// A negative value overhangs the area above.
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: [u8; 3],
}

impl StatefulWidget for Bar {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        let x = cells::to_sixteenths(area.x, self.x);
        let y = cells::to_sixteenths(area.y, self.y);

        command::encode_bar_into(
            scene.buffer(),
            &BarCommand {
                x,
                y,
                width: self.width,
                height: self.height,
                color: self.color,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Bar;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_bar, BarCommand};

    #[test]
    fn emits_a_bar_at_absolute_sixteenths() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        Bar {
            x: 3,
            y: 0,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        }
        .render(Rect::new(2, 4, 1, 1), &mut buf, &mut scene);

        let expected = encode_bar(&BarCommand {
            x: 2 * 16 + 3,
            y: 4 * 16,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn a_negative_anchor_overhangs_the_area() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        Bar {
            x: -4,
            y: -16,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        }
        .render(Rect::new(2, 4, 1, 1), &mut buf, &mut scene);

        let expected = encode_bar(&BarCommand {
            x: 2 * 16 - 4,
            y: 4 * 16 - 16,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        });
        assert_eq!(
            scene.buffer().as_slice(),
            expected.as_slice(),
            "the bar sits left of and above the area origin"
        );
    }
}
