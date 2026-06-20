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
/// in any other terminal. `size` shapes only the rich icon, not the one-cell
/// fallback.
pub struct Icon {
    pub kind: IconKind,
    pub color: [u8; 3],
    pub size: u8,
}

impl StatefulWidget for Icon {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        let [r, g, b] = self.color;
        cells::put(
            buf,
            area.x,
            area.y,
            sigil(self.kind),
            Style::default().fg(Color::Rgb(r, g, b)),
        );

        command::encode_icon_into(
            scene.buffer(),
            &IconCommand {
                top: area.y,
                left: area.x,
                kind: self.kind,
                color: self.color,
                size: self.size,
            },
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
        }
        .render(area, &mut buf, &mut scene);

        let expected = encode_icon(&IconCommand {
            top: 7,
            left: 5,
            kind: IconKind::Warning,
            color: [229, 192, 123],
            size: 2,
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
            }
            .render(area, &mut buf, &mut scene);
            buf.cell((0u16, 0u16)).expect("cell").symbol().to_owned()
        };

        assert_eq!(render_kind(IconKind::Error), "E");
        assert_eq!(render_kind(IconKind::Warning), "W");
        assert_eq!(render_kind(IconKind::Info), "I");
    }
}
