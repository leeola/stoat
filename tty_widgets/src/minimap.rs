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
pub struct Minimap {
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
    pub palette: Vec<[u8; 3]>,
}

impl StatefulWidget for Minimap {
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

#[cfg(test)]
mod tests {
    use super::Minimap;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_minimap, MinimapCommand};

    fn config() -> Minimap {
        Minimap {
            strip_id: 3,
            content_id: 7,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [40, 44, 52, 0],
            thumb: [99, 109, 131, 64],
            thumb_border: [60, 66, 77],
            palette: vec![[224, 108, 117], [152, 195, 121]],
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
            palette: vec![[224, 108, 117], [152, 195, 121]],
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
}
