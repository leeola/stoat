use crate::{
    render::paint::{dim_rgb, style_rgb},
    theme::{scope, Theme},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::StatefulWidget};
use stoat_widgets::{bar::Bar, ApcScene};

/// The cells the Blame Column takes at the left of a blamed pane.
///
/// The BlameView board draws the column `width: 158px`, which is 17 and a half
/// cells at the scene's 9 px cell. The column reserves whole cells, so the text
/// it holds stays on the cell grid.
pub(crate) const BLAME_COLUMN_CELLS: u16 = 18;

/// The width of the column's surface, in sixteenths of a cell.
///
/// 17 cells and 9 sixteenths is the board's 158 px at the scene's 9 px cell.
/// The rest of the column's last cell shows the editor surface, which is the
/// board's 4 px between the column and the gutter.
pub(crate) const BLAME_SURFACE_WIDTH: u16 = 281;

/// The text cells that a pane keeps beside the column and the gutter.
///
/// A narrower pane shows no column, so a pane split small still shows its
/// text.
pub(crate) const BLAME_MIN_TEXT_CELLS: u16 = 20;

/// The colors the Blame Column draws in, resolved once per theme.
#[derive(Clone)]
pub(crate) struct BlameColors {
    pub(crate) surface: [u8; 3],
}

impl BlameColors {
    /// The column's colors in `theme`, or `None` when the theme holds no RGB
    /// surface for it.
    ///
    /// The surface is the `ui.blame.column` background. A theme that leaves it
    /// unset fills the column with the `ui.surface.raised` background, and
    /// with the `ui.background` background when that is unset too.
    pub(crate) fn resolve(theme: &Theme) -> Option<BlameColors> {
        [
            scope::UI_BLAME_COLUMN,
            scope::UI_SURFACE_RAISED,
            scope::UI_BACKGROUND,
        ]
        .into_iter()
        .find_map(|name| style_rgb(theme.try_get(name)?.bg))
        .map(|surface| BlameColors { surface })
    }

    /// These colors blended toward `bg` by `amount`, as an unfocused pane
    /// fades its gutter.
    pub(crate) fn dim(&self, bg: [u8; 3], amount: f32) -> BlameColors {
        BlameColors {
            surface: dim_rgb(self.surface, bg, amount),
        }
    }
}

/// The cells the Blame Column takes in a pane `width` cells wide whose gutter
/// takes `gutter`.
///
/// 0 when the pane has no room for the column, the gutter, and
/// [`BLAME_MIN_TEXT_CELLS`] of text.
pub(crate) fn blame_cells(width: u16, gutter: u16) -> u16 {
    let needed = BLAME_COLUMN_CELLS
        .saturating_add(gutter)
        .saturating_add(BLAME_MIN_TEXT_CELLS);
    match width >= needed {
        true => BLAME_COLUMN_CELLS,
        false => 0,
    }
}

/// Paint the surface of the Blame Column in `area` over its first `rows` rows.
///
/// With a live scene the surface is one bar [`BLAME_SURFACE_WIDTH`] sixteenths
/// wide, which writes no cell. Without one, the surface color fills the
/// background of the column's cells but the last, which shows the editor
/// surface as the bar's last sixteenths do. The cells outside `buf` take no
/// write.
pub(crate) fn paint_blame_surface(
    area: Rect,
    rows: u16,
    colors: &BlameColors,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    let rows = rows.min(area.height);
    if let Some(scene) = scene.filter(|scene| scene.live()) {
        Bar {
            x: 0,
            y: 0,
            width: BLAME_SURFACE_WIDTH,
            height: rows.saturating_mul(16),
            color: colors.surface,
            radius: 0,
            corners: 0,
        }
        .render(area, buf, scene);
        return;
    }

    let [r, g, b] = colors.surface;
    let cells = Rect {
        width: area.width.min(BLAME_COLUMN_CELLS - 1),
        height: rows,
        ..area
    }
    .intersection(buf.area);
    for y in cells.top()..cells.bottom() {
        for x in cells.left()..cells.right() {
            buf[(x, y)].set_bg(Color::Rgb(r, g, b));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{blame_cells, BlameColors, BLAME_COLUMN_CELLS, BLAME_MIN_TEXT_CELLS};
    use crate::{app::DEFAULT_KEYMAP, theme::Theme};

    /// The ermine theme fills the column with its own surface, and
    /// default_dark, which sets no column or raised surface, with its
    /// background.
    #[test]
    fn the_surface_falls_back_to_the_raised_surface_then_the_background() {
        let config = stoat_config::parse(DEFAULT_KEYMAP)
            .0
            .expect("the shipped config parses");
        let surface = |name| {
            let theme = Theme::from_config(&config, name).expect("the shipped theme loads");
            BlameColors::resolve(&theme).map(|colors| colors.surface)
        };
        assert_eq!(
            [surface("ermine_night"), surface("default_dark")],
            [Some([0x14, 0x16, 0x1a]), Some([0x28, 0x2c, 0x34])],
        );
    }

    /// The column shows in a pane with room for it, the gutter, and the text
    /// cells it keeps, and in no narrower pane.
    #[test]
    fn the_column_needs_room_for_the_gutter_and_the_text() {
        let gutter = 5;
        let fits = BLAME_COLUMN_CELLS + gutter + BLAME_MIN_TEXT_CELLS;
        assert_eq!(
            [blame_cells(fits, gutter), blame_cells(fits - 1, gutter)],
            [BLAME_COLUMN_CELLS, 0],
        );
    }
}
