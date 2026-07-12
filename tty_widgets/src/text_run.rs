use crate::ApcScene;
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command;

/// A run of text drawn at a fractional scale, off the cell grid.
///
/// The run is drawn off the cell grid, so it can be smaller than the grid (a
/// gutter line number) yet still line up with full-size rows. [`Self::col`] and
/// [`Self::row`] are the anchor in **sixteenths of a cell** relative to the
/// render area's top-left; [`Self::scale`] is the glyph size in **256ths of the
/// cell size** (256 = grid size). The run advances one scaled cell width per
/// character and is vertically centered within its row. There is no cell
/// fallback: the run is inherently sub-cell.
///
/// `text` is borrowed so a caller can pass a slice of a reused buffer (a gutter
/// formats line numbers into a stack buffer) rather than own a string per frame.
pub struct TextRun<'a> {
    /// Left anchor in sixteenths, from the area's left.
    pub col: u16,
    /// Row anchor in sixteenths, from the area's top.
    pub row: u16,
    /// Glyph size in 256ths of the cell size.
    pub scale: u16,
    pub color: [u8; 3],
    /// Opaque background box the run composites over, or `None` to blend the
    /// glyphs directly over the surface behind the run with no backing box.
    pub bg: Option<[u8; 3]>,
    pub text: &'a str,
}

impl StatefulWidget for TextRun<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        let col = area.x as i16 * 16 + self.col as i16;
        let row = area.y as i16 * 16 + self.row as i16;

        command::encode_text_run_into(
            scene.buffer(),
            col,
            row,
            self.scale,
            self.color,
            self.bg,
            self.text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::TextRun;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_text_run, TextRunCommand};

    #[test]
    fn emits_a_run_at_absolute_sixteenths() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        TextRun {
            col: 4,
            row: 0,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            text: "42",
        }
        .render(Rect::new(3, 5, 2, 1), &mut buf, &mut scene);

        let expected = encode_text_run(&TextRunCommand {
            col: 3 * 16 + 4,
            row: 5 * 16,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            text: "42".to_owned(),
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }
}
