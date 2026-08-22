use crate::{cells, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, TextRunCommand};

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
    /// Left anchor in sixteenths, from the area's left. A negative value
    /// starts the run left of the area.
    pub col: i16,
    /// Row anchor in sixteenths, from the area's top. A negative value lifts
    /// the run above the area.
    pub row: i16,
    /// Glyph size in 256ths of the cell size.
    pub scale: u16,
    pub color: [u8; 3],
    /// Opaque background box the run composites over, or `None` to blend the
    /// glyphs directly over the surface behind the run with no backing box.
    pub bg: Option<[u8; 3]>,
    pub text: &'a str,
    /// Hand-drawn mark whose reveal this run fades in with, by its id, so a
    /// label appears as the mark it names draws itself. Zero draws the run at
    /// full alpha the moment it is declared.
    pub follow: u32,
    /// Pool this run rides, and that pool's top row, so the run glides with a
    /// scrolling pane. `None` leaves it fixed to the screen.
    pub anchor: Option<(u32, f32)>,
}

/// Sixteenths a run of `chars` advances at `scale`, the glyph size in 256ths of
/// a cell.
///
/// Every component that measures scaled text shares this one rounding, so a
/// gutter number, a status segment, and a popup line placed against each other
/// agree on where the run ends. Half a sixteenth rounds up, which is the nearer
/// of the two positions.
///
/// The product overflows a u16 long before the quotient does, so the widening
/// matters even for advances that comfortably fit the result. Takes the
/// character count directly rather than a narrowed one, since a truncated count
/// corrupts the answer before any arithmetic runs.
pub fn advance_sixteenths(chars: usize, scale: u16) -> u16 {
    let total = (chars as u64)
        .saturating_mul(u64::from(scale))
        .saturating_add(8)
        / 16;
    total.min(u64::from(u16::MAX)) as u16
}

impl StatefulWidget for TextRun<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        let col = cells::to_sixteenths(area.x, self.col);
        let row = cells::to_sixteenths(area.y, self.row);

        command::encode_text_run_into(
            scene.buffer(),
            &TextRunCommand {
                col,
                row,
                scale: self.scale,
                color: self.color,
                bg: self.bg,
                follow: self.follow,
                anchor: self.anchor,
                text: self.text,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{advance_sixteenths, TextRun};
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
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(3, 5, 2, 1), &mut buf, &mut scene);

        let expected = encode_text_run(&TextRunCommand {
            col: 3 * 16 + 4,
            row: 5 * 16,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            follow: 0,
            anchor: None,
            text: "42".to_owned(),
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    /// A label names the mark it fades in with and the pool it rides. Both are
    /// terminal-side ids rather than anything area-relative, so they reach the
    /// wire as declared.
    #[test]
    fn a_follow_and_an_anchor_reach_the_wire_as_declared() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: [99, 109, 131],
            bg: None,
            text: "42",
            follow: 7,
            anchor: Some((3, 12.5)),
        }
        .render(Rect::new(1, 1, 2, 1), &mut buf, &mut scene);

        let expected = encode_text_run(&TextRunCommand {
            col: 16,
            row: 16,
            scale: 256,
            color: [99, 109, 131],
            bg: None,
            follow: 7,
            anchor: Some((3, 12.5)),
            text: "42".to_owned(),
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn a_negative_anchor_overhangs_the_area() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        TextRun {
            col: -8,
            row: -16,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            text: "42",
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(3, 5, 2, 1), &mut buf, &mut scene);

        let expected = encode_text_run(&TextRunCommand {
            col: 3 * 16 - 8,
            row: 5 * 16 - 16,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            follow: 0,
            anchor: None,
            text: "42".to_owned(),
        });
        assert_eq!(
            scene.buffer().as_slice(),
            expected.as_slice(),
            "the run starts left of and above the area origin"
        );
    }

    /// The product overflows a u16 well before the quotient does, so a legal
    /// advance is the interesting case, not a saturating one.
    #[test]
    fn a_long_run_advances_without_overflowing_the_product() {
        assert_eq!(
            advance_sixteenths(410, 160),
            4100,
            "65600 before the divide"
        );
        assert_eq!(
            advance_sixteenths(usize::MAX, 256),
            u16::MAX,
            "and saturates"
        );
    }

    #[test]
    fn half_a_sixteenth_rounds_up() {
        assert_eq!(advance_sixteenths(1, 218), 14, "218/16 is 13.625");
        assert_eq!(advance_sixteenths(1, 8), 1, "exactly half rounds up");
        assert_eq!(advance_sixteenths(1, 7), 0, "just under half rounds down");
    }
}
