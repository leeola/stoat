use crate::{cells, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, PolylineCommand};

/// A stroked path drawn off the cell grid.
///
/// The one widget whose geometry is not axis-aligned, added so a commit graph
/// can draw lane and merge lines. Every coordinate and [`Self::width`] is in
/// **sixteenths of a cell** (16 = one cell) relative to the render area's
/// top-left, so a path tracks live font zoom like a [`crate::bar::Bar`].
///
/// A single point, or two equal ones, draws a dot. There is no cell fallback: a
/// stroked path is inherently sub-cell, so a caller wanting glyphs writes them
/// instead of rendering this.
pub struct Polyline {
    /// Vertices in draw order, each `[x, y]` in sixteenths from the area's
    /// top-left.
    pub points: Vec<[i16; 2]>,
    /// Stroke thickness in sixteenths of the cell's width, centered on the
    /// path. Measured against the width on both axes, so a diagonal is as thick
    /// as a vertical and 16 draws exactly one column wide.
    pub width: u16,
    pub color: [u8; 3],
}

impl StatefulWidget for Polyline {
    type State = ApcScene;

    fn render(mut self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        for point in &mut self.points {
            *point = [
                cells::to_sixteenths(area.x, point[0]),
                cells::to_sixteenths(area.y, point[1]),
            ];
        }

        command::encode_polyline_into(
            scene.buffer(),
            &PolylineCommand {
                points: self.points,
                width: self.width,
                color: self.color,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Polyline;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_polyline, PolylineCommand};

    #[test]
    fn emits_a_path_at_absolute_sixteenths() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        Polyline {
            points: vec![[3, 0], [3, 16]],
            width: 4,
            color: [10, 20, 30],
        }
        .render(Rect::new(2, 1, 10, 4), &mut buf, &mut scene);

        assert_eq!(
            scene.buffer(),
            &encode_polyline(&PolylineCommand {
                points: vec![[2 * 16 + 3, 16], [2 * 16 + 3, 16 + 16]],
                width: 4,
                color: [10, 20, 30],
            }),
            "every point shifts by the area origin"
        );
    }
}
