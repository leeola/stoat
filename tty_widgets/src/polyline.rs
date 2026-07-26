use crate::ApcScene;
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
    /// Stroke thickness in sixteenths, centered on the path.
    pub width: u16,
    pub color: [u8; 3],
}

impl StatefulWidget for Polyline {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        let origin = [area.x as i16 * 16, area.y as i16 * 16];
        let points = self
            .points
            .iter()
            .map(|p| [origin[0] + p[0], origin[1] + p[1]])
            .collect();

        command::encode_polyline_into(
            scene.buffer(),
            &PolylineCommand {
                points,
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
