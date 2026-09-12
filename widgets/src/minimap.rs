use crate::ApcScene;
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, MinimapCommand};

/// Declare the render area as a minimap strip rendering a whole buffer.
///
/// Emits a `minimap` APC frame naming the strip's geometry and rendering
/// parameters. A stoatty terminal draws the buffer's per-line run summaries down
/// the strip, overlaid with a viewport thumb. The summaries arrive out of band
/// as `minimap_lines` keyed by [`Self::content_id`], so a redeclared strip keeps
/// its content.
///
/// The strip has no cell fallback. Any other terminal leaves the reserved
/// cells blank, degrading to no minimap.
pub struct Minimap<'a> {
    /// Names this declaration, so its view thumb and any redeclare address it.
    pub strip_id: u32,
    /// The line-summary store the strip renders, updated by `minimap_lines`.
    pub content_id: u32,
    /// Buffer lines drawn per vertical cell.
    pub lines_per_cell: u8,
    /// Widest line, in minimap columns, the strip renders before clipping.
    pub max_columns: u8,
    /// Strip background as rgba. A zero alpha lets the editor body show through.
    pub bg: [u8; 4],
    /// Viewport-thumb fill, rgba.
    pub thumb: [u8; 4],
    /// Viewport-thumb outline, rgb.
    pub thumb_border: [u8; 3],
    /// Run-class palette, up to 64 rgb entries a summary's classes index.
    ///
    /// Borrowed, since a strip is redeclared every frame and the palette it
    /// names outlives the declaration.
    pub palette: &'a [[u8; 3]],
}

impl StatefulWidget for Minimap<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        command::encode_minimap_into(
            scene.buffer(),
            &MinimapCommand {
                top: area.y,
                left: area.x,
                width: area.width,
                height: area.height,
                strip_id: self.strip_id,
                content_id: self.content_id,
                lines_per_cell: self.lines_per_cell,
                max_columns: self.max_columns,
                bg: self.bg,
                thumb: self.thumb,
                thumb_border: self.thumb_border,
                palette: self.palette,
            },
        );
    }
}

/// The first buffer line the strip renders, tracking the editor's viewport.
///
/// A strip that can show `visible_lines` lines slides its window across the file
/// in proportion to how far the viewport (`view_top` over the scrollable
/// `total - view_visible` span) has scrolled, mapping the whole file onto the
/// strip. Returns 0 when the file fits the strip.
pub fn minimap_top(total: f32, visible_lines: f32, view_top: f32, view_visible: f32) -> f32 {
    if total <= visible_lines {
        return 0.0;
    }
    let scrollable = total - view_visible;
    let ratio = if scrollable > 0.0 {
        (view_top / scrollable).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ratio * (total - visible_lines)
}

/// The buffer line a pointer at strip cell-row `row` (0-based from the strip top)
/// points at, for a strip `strip_rows` cells tall over the given viewport.
///
/// The strip shows `strip_rows * lines_per_cell` lines from [`minimap_top`], and
/// each cell spans `lines_per_cell` lines, so the click lands on that cell's
/// middle line. Pass the `lines_per_cell` the strip was declared with, so the
/// pointer resolves against the window the terminal drew. A row past the strip
/// clamps to its last cell.
pub fn click_target_line(
    strip_rows: u16,
    lines_per_cell: u8,
    row: u16,
    total: f32,
    view_top: f32,
    view_visible: f32,
) -> u32 {
    let lines_per_cell = f32::from(lines_per_cell);
    let visible_lines = strip_rows as f32 * lines_per_cell;
    let top = minimap_top(total, visible_lines, view_top, view_visible);
    let row = row.min(strip_rows.saturating_sub(1)) as f32;
    (top + row * lines_per_cell + lines_per_cell / 2.0).max(0.0) as u32
}

#[cfg(test)]
mod tests {
    use super::{click_target_line, minimap_top, Minimap};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_minimap, MinimapCommand};

    const PALETTE: [[u8; 3]; 2] = [[224, 108, 117], [152, 195, 121]];

    fn config() -> Minimap<'static> {
        Minimap {
            strip_id: 3,
            content_id: 7,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [40, 44, 52, 0],
            thumb: [99, 109, 131, 64],
            thumb_border: [60, 66, 77],
            palette: &PALETTE,
        }
    }

    #[test]
    fn emits_a_minimap_declare_over_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(72, 0, 8, 20);
        let mut buf = Buffer::empty(area);

        config().render(area, &mut buf, &mut scene);

        let expected = encode_minimap(&MinimapCommand {
            top: 0,
            left: 72,
            width: 8,
            height: 20,
            strip_id: 3,
            content_id: 7,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [40, 44, 52, 0],
            thumb: [99, 109, 131, 64],
            thumb_border: [60, 66, 77],
            palette: PALETTE.to_vec(),
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn writes_no_cell_fallback() {
        let mut scene = ApcScene::new();
        let area = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(area);

        config().render(area, &mut buf, &mut scene);

        assert_eq!(
            buf,
            Buffer::empty(area),
            "the strip stays blank for the GPU minimap pass"
        );
    }

    #[test]
    fn minimap_top_maps_the_viewport_across_the_file() {
        assert_eq!(minimap_top(40.0, 120.0, 0.0, 30.0), 0.0, "a fitted file");

        let mid = minimap_top(200.0, 80.0, 85.0, 30.0);
        assert!(
            (mid - 60.0).abs() < 1e-4,
            "half-scrolled lands mid-strip: {mid}"
        );

        assert_eq!(
            minimap_top(200.0, 80.0, 1_000.0, 30.0),
            120.0,
            "a view past the end clamps to the span bottom"
        );
    }

    #[test]
    fn click_target_line_centers_within_the_cell_row() {
        assert_eq!(
            click_target_line(10, 8, 3, 50.0, 0.0, 20.0),
            28,
            "cell row 3 of a fitted file centers on line 3*8+4"
        );
        assert_eq!(
            click_target_line(10, 8, 40, 50.0, 0.0, 20.0),
            76,
            "a row past the strip clamps to the last cell"
        );

        let top = minimap_top(800.0, 80.0, 400.0, 20.0);
        assert_eq!(
            click_target_line(10, 8, 0, 800.0, 400.0, 20.0),
            (top + 4.0) as u32,
            "a slid window shifts the target by minimap_top"
        );
    }
}
