use crate::{cells, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::StatefulWidget,
};
use stoatty_protocol::command::{self, IconCommand, IconKind};

/// A status icon composited at a grid cell.
///
/// Emits an `icon` APC frame so a stoatty terminal draws the [`IconKind`]
/// silhouette crisp at any `size`, and writes a single representative letter into
/// the cell at the area's top-left so the same frame degrades to a severity mark
/// in any other terminal. `size` and `offset` shape only the rich icon, not the
/// one-cell fallback.
pub struct Icon {
    pub kind: IconKind,
    pub color: [u8; 3],
    pub size: u8,
    /// Signed `[x, y]` pixel offset from the anchor cell, so the icon can align
    /// with a popover's inset content instead of snapping to the grid.
    pub offset: [i16; 2],
}

impl StatefulWidget for Icon {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);
        self.draw_components(area, scene);
    }
}

impl Icon {
    /// Draw only the off-grid icon.
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded severity
    /// letter and so leaves it showing under the silhouette inside a rich
    /// terminal. Writes no cells.
    pub fn draw_components(&self, area: Rect, scene: &mut ApcScene) {
        command::encode_icon_into(
            scene.buffer(),
            &IconCommand {
                top: area.y,
                left: area.x,
                kind: self.kind,
                color: self.color,
                size: self.size,
                offset: self.offset,
            },
        );
    }

    /// Draw only the degraded severity letter, for a terminal without the
    /// off-grid icon.
    ///
    /// One cell at the area's top-left. [`Self::size`] and [`Self::offset`]
    /// shape the rich icon alone, so neither reaches the cell.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let [r, g, b] = self.color;
        cells::put(
            buf,
            area.x,
            area.y,
            sigil(self.kind),
            Style::default().fg(Color::Rgb(r, g, b)),
        );
    }
}

/// The fallback cell letter for each severity, matching the editor gutter's
/// severity marks so a diagnostics scene degrades the same way.
fn sigil(kind: IconKind) -> &'static str {
    match kind {
        IconKind::Error => "E",
        IconKind::Warning => "W",
        IconKind::Info => "I",
    }
}

#[cfg(test)]
mod tests {
    use super::Icon;
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_icon, IconCommand, IconKind};

    #[test]
    fn emits_an_icon_frame_at_the_area() {
        let mut scene = ApcScene::new();
        let area = Rect::new(5, 7, 1, 1);
        let mut buf = Buffer::empty(area);

        Icon {
            kind: IconKind::Warning,
            color: [229, 192, 123],
            size: 2,
            offset: [3, 6],
        }
        .render(area, &mut buf, &mut scene);

        let expected = encode_icon(&IconCommand {
            top: 7,
            left: 5,
            kind: IconKind::Warning,
            color: [229, 192, 123],
            size: 2,
            offset: [3, 6],
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn draws_a_severity_sigil_fallback() {
        let render_kind = |kind| {
            let mut scene = ApcScene::new();
            let area = Rect::new(0, 0, 1, 1);
            let mut buf = Buffer::empty(area);
            Icon {
                kind,
                color: [1, 2, 3],
                size: 1,
                // A non-zero offset shapes only the rich frame. The sigil
                // fallback ignores it and still lands at the cell.
                offset: [3, 6],
            }
            .render(area, &mut buf, &mut scene);
            buf.cell((0u16, 0u16)).expect("cell").symbol().to_owned()
        };

        assert_eq!(render_kind(IconKind::Error), "E");
        assert_eq!(render_kind(IconKind::Warning), "W");
        assert_eq!(render_kind(IconKind::Info), "I");
    }

    /// An app compositing its own chrome takes the halves apart. The icon half
    /// must leave the buffer alone, or the severity letter shows through
    /// beneath the silhouette.
    #[test]
    fn the_two_halves_split_cleanly() {
        let area = Rect::new(0, 0, 1, 1);
        let icon = Icon {
            kind: IconKind::Warning,
            color: [1, 2, 3],
            size: 2,
            offset: [3, 6],
        };

        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(area);
        icon.draw_components(area, &mut scene);
        assert!(!scene.bytes().is_empty(), "the icon is emitted");
        assert_eq!(buf, Buffer::empty(area), "and no cell is written");

        let scene = ApcScene::new();
        icon.draw_fallback(area, &mut buf);
        assert_eq!(
            buf.cell((0u16, 0u16)).expect("cell").symbol(),
            "W",
            "the fallback writes cells",
        );
        assert!(scene.bytes().is_empty(), "and emits nothing");
    }
}
