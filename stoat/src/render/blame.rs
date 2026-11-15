use crate::{
    blame::{self, BlameBase, BlameRows},
    render::paint::{dim_rgb, style_rgb},
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use stoat_widgets::{
    bar::{Bar, BarBatch},
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};

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

/// The width of a run's bar, in sixteenths of a cell, which is the board's
/// `width: 2px` at the scene's 9 px cell.
const RUN_BAR_WIDTH: u16 = 4;

/// Where a run's label starts, in sixteenths from the column's left edge,
/// which is past the board's 2 px bar and its 6 px gap.
const LABEL_START: u16 = 14;

/// Where a run's label ends at the latest, in sixteenths from the column's
/// left edge, which leaves the board's 8 px right pad.
const LABEL_END: u16 = 267;

/// The last cell a run's label writes in, from the column's left edge, when
/// the column draws in cells.
const LABEL_CELL_END: u16 = 16;

/// The colors the Blame Column draws in, resolved once per theme.
#[derive(Clone)]
pub(crate) struct BlameColors {
    pub(crate) surface: [u8; 3],
    /// The bar and the short sha of a run, by its commit's age bucket, oldest
    /// first.
    pub(crate) ages: [[u8; 3]; 4],
    /// The line over the first row of each run past the file's first line.
    pub(crate) divider: [u8; 3],
    pub(crate) author: [u8; 3],
    pub(crate) date: [u8; 3],
    /// The bar and the label of an uncommitted run.
    pub(crate) uncommitted: [u8; 3],
    /// The bar and the short sha of a run of the selected commit.
    pub(crate) selected: [u8; 3],
    /// The surface beside the lines of the selected commit.
    pub(crate) selected_surface: [u8; 3],
    pub(crate) selected_author: [u8; 3],
    pub(crate) selected_date: [u8; 3],
    /// The glyph size of a run's label, in 256ths of a cell.
    pub(crate) scale: u16,
}

/// The blame that the column paints its runs from.
pub(crate) struct BlameLines<'a> {
    pub(crate) rows: &'a BlameRows,
    /// `None` for a file HEAD does not hold, whose runs are all uncommitted.
    pub(crate) base: Option<&'a BlameBase>,
    /// The moment the frame paints, which a run's date measures its age
    /// against.
    pub(crate) now_epoch: i64,
    /// The index of the commit under the focused pane's cursor, whose runs
    /// draw as the selection, or `None` to select nothing.
    pub(crate) selected: Option<u32>,
}

/// The colors of one line of the column.
///
/// The lines of the selected commit take the selection's colors, and the
/// others take their commit's age color and the plain surface and label.
struct LinePalette {
    bar: [u8; 3],
    surface: [u8; 3],
    author: [u8; 3],
    date: [u8; 3],
}

/// What a blamed pane's Blame Column paints.
pub(crate) struct BlamePaint<'a> {
    pub(crate) colors: &'a BlameColors,
    /// `None` before a blame of the buffer lands, and the column then paints
    /// its surface alone.
    pub(crate) lines: Option<BlameLines<'a>>,
}

impl BlameColors {
    /// The column's colors in `theme`, or `None` when the theme holds no RGB
    /// color for the surface, the divider, the author, or the muted text.
    ///
    /// The surface is the `ui.blame.column` background. A theme that leaves it
    /// unset fills the column with the `ui.surface.raised` background, and
    /// with the `ui.background` background when that is unset too. An age the
    /// theme leaves unset draws in the `ui.text.muted` foreground.
    ///
    /// The selection takes `ui.blame.selected`, its date
    /// `ui.blame.selected.date`, and its author `ui.text`. A theme that leaves
    /// them unset selects with the `ui.text` foreground and the `ui.selection`
    /// background. A selected color that resolves to no RGB at all falls back
    /// to the author color, and the selected surface to the surface.
    pub(crate) fn resolve(theme: &Theme) -> Option<BlameColors> {
        let fg = |name: &str| style_rgb(theme.try_get(name)?.fg);
        let bg = |name: &str| style_rgb(theme.try_get(name)?.bg);
        let surface = [
            scope::UI_BLAME_COLUMN,
            scope::UI_SURFACE_RAISED,
            scope::UI_BACKGROUND,
        ]
        .into_iter()
        .find_map(bg)?;
        let muted = fg(scope::UI_TEXT_MUTED)?;
        let author = fg(scope::UI_TEXT_DIM)?;
        let text = fg(scope::UI_TEXT).unwrap_or(author);

        Some(BlameColors {
            surface,
            ages: scope::UI_BLAME_AGE.map(|name| fg(name).unwrap_or(muted)),
            divider: fg(scope::UI_DIVIDER)?,
            author,
            date: muted,
            uncommitted: muted,
            selected: fg(scope::UI_BLAME_SELECTED).unwrap_or(text),
            selected_surface: [scope::UI_BLAME_SELECTED, scope::UI_SELECTION]
                .into_iter()
                .find_map(bg)
                .unwrap_or(surface),
            selected_author: text,
            selected_date: fg(scope::UI_BLAME_SELECTED_DATE).unwrap_or(text),
            scale: theme.type_scale(TypeRole::Small),
        })
    }

    /// These colors blended toward `bg` by `amount`, as an unfocused pane
    /// fades its gutter. The scale stays.
    pub(crate) fn dim(&self, bg: [u8; 3], amount: f32) -> BlameColors {
        let dim = |color: [u8; 3]| dim_rgb(color, bg, amount);
        BlameColors {
            surface: dim(self.surface),
            ages: self.ages.map(dim),
            divider: dim(self.divider),
            author: dim(self.author),
            date: dim(self.date),
            uncommitted: dim(self.uncommitted),
            selected: dim(self.selected),
            selected_surface: dim(self.selected_surface),
            selected_author: dim(self.selected_author),
            selected_date: dim(self.selected_date),
            scale: self.scale,
        }
    }

    /// The colors of a line of a run of `commit`, the selection's when the
    /// commit is the one `lines` selects.
    fn palette(&self, lines: &BlameLines<'_>, commit: Option<u32>) -> LinePalette {
        match commit.is_some() && commit == lines.selected {
            true => LinePalette {
                bar: self.selected,
                surface: self.selected_surface,
                author: self.selected_author,
                date: self.selected_date,
            },
            false => LinePalette {
                bar: run_color(lines, commit, self),
                surface: self.surface,
                author: self.author,
                date: self.date,
            },
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

/// Paint the Blame Column in `area` for the `folded` lines that the view
/// shows.
///
/// `folded` holds each line's 1-based number and its height in rows, as
/// [`gutter_geometry`](crate::render::editor::gutter_geometry) folds them. A
/// line takes a bar in its commit's age color the whole height of the line,
/// and the surface beside it. The first line of a run takes the run's label,
/// and a divider over its top when it is not the file's first line. A line
/// with no run, before a blame lands or past the last line, takes the surface
/// alone.
///
/// The lines of the commit that `lines` selects take the selection's bar,
/// surface, and label colors. The divider over them keeps its color.
///
/// With a live scene the column draws as bars and text runs and writes no
/// cell. Without one, the surface fills the column's cells but the last, and
/// the first line of a run writes its short sha and author into them.
pub(crate) fn paint_blame_column(
    lines: Option<&BlameLines<'_>>,
    folded: &[(u32, u16)],
    area: Rect,
    colors: &BlameColors,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    match scene.filter(|scene| scene.live()) {
        Some(scene) => paint_components(lines, folded, area, colors, scene),
        None => paint_cells(lines, folded, area, colors, buf),
    }
}

/// The column as bars and text runs in `scene`.
///
/// The bars of one line meet edge to edge and never overlap, so a
/// [`BarBatch`] that groups them by color draws what they draw one by one.
/// The labels go out after the bars, so they draw over the surface.
fn paint_components(
    lines: Option<&BlameLines<'_>>,
    folded: &[(u32, u16)],
    area: Rect,
    colors: &BlameColors,
    scene: &mut ApcScene,
) {
    let mut bars = BarBatch::default();
    let mut labels = TextRunBatch::default();
    let mut top = 0u16;
    for &(number, height) in folded {
        let (y, span) = ((top * 16) as i16, height * 16);
        let line = number.saturating_sub(1);
        top += height;

        let Some((lines, (rows, commit))) =
            lines.and_then(|lines| Some((lines, lines.rows.run(line)?)))
        else {
            bars.push(area, bar(0, y, BLAME_SURFACE_WIDTH, span, colors.surface));
            continue;
        };

        let starts = rows.start == line;
        let divider = starts && line > 0;
        let palette = colors.palette(lines, *commit);
        let surface_width = BLAME_SURFACE_WIDTH - RUN_BAR_WIDTH;
        bars.push(area, bar(0, y, RUN_BAR_WIDTH, span, palette.bar));
        if divider {
            bars.push(
                area,
                bar(RUN_BAR_WIDTH, y, surface_width, 1, colors.divider),
            );
        }
        let lead = u16::from(divider);
        bars.push(
            area,
            bar(
                RUN_BAR_WIDTH,
                y + lead as i16,
                surface_width,
                span - lead,
                palette.surface,
            ),
        );

        if starts {
            let room = usize::from(LABEL_END - LABEL_START) * 16 / usize::from(colors.scale.max(1));
            let mut chars = 0;
            for (text, color, italic) in label_parts(lines, *commit, &palette, colors, room) {
                let len = text.chars().count();
                if chars + len > room {
                    break;
                }
                let col = LABEL_START + text_run::advance_sixteenths(chars, colors.scale);
                labels.push(
                    area,
                    TextRun {
                        col: col as i16,
                        row: y,
                        scale: colors.scale,
                        color,
                        bg: None,
                        bold: false,
                        italic,
                        text: &text,
                        follow: 0,
                        anchor: None,
                    },
                );
                chars += len + 1;
            }
        }
    }
    bars.flush(scene);
    labels.flush(scene);
}

/// A square bar at `x` and `y` from the column's top left, in sixteenths.
fn bar(x: u16, y: i16, width: u16, height: u16, color: [u8; 3]) -> Bar {
    Bar {
        x: x as i16,
        y,
        width,
        height,
        color,
        radius: 0,
        corners: 0,
    }
}

/// The column as cells in `buf`, for a terminal that draws no components.
///
/// Each line's cells but the column's last take the surface of its palette,
/// and the first line of a run writes its short sha and author, or
/// `uncommitted`, from the column's second cell.
fn paint_cells(
    lines: Option<&BlameLines<'_>>,
    folded: &[(u32, u16)],
    area: Rect,
    colors: &BlameColors,
    buf: &mut Buffer,
) {
    let column = Rect {
        width: area.width.min(BLAME_COLUMN_CELLS - 1),
        ..area
    }
    .intersection(buf.area);
    let mut top = 0u16;
    for &(number, height) in folded {
        let (y, line) = (area.y + top, number.saturating_sub(1));
        top += height;

        let run = lines.and_then(|lines| Some((lines, lines.rows.run(line)?)));
        let palette = run.map(|(lines, (_, commit))| colors.palette(lines, *commit));
        let surface = rgb(palette
            .as_ref()
            .map_or(colors.surface, |palette| palette.surface));
        let cells = Rect {
            y,
            height,
            ..column
        }
        .intersection(column);
        for cy in cells.top()..cells.bottom() {
            for cx in cells.left()..cells.right() {
                buf[(cx, cy)].set_bg(surface);
            }
        }

        let (Some((lines, (span, commit))), Some(palette)) = (run, palette) else {
            continue;
        };
        if span.start != line || cells.is_empty() {
            continue;
        }
        let style = |color: [u8; 3]| Style::default().fg(rgb(color)).bg(surface);
        let label = match commit {
            None => vec![(
                "uncommitted",
                style(colors.uncommitted).add_modifier(Modifier::ITALIC),
            )],
            Some(index) => {
                let Some(entry) = lines
                    .base
                    .and_then(|base| base.blame.commits.get(*index as usize))
                else {
                    continue;
                };
                let author = style(palette.author);
                vec![
                    (entry.info.short_sha.as_str(), style(palette.bar)),
                    (" ", author),
                    (blame::author_word(&entry.info.author_name), author),
                ]
            },
        };

        let mut x = area.x + 1;
        let end = (area.x + LABEL_CELL_END).min(cells.right());
        for (text, style) in label {
            x = buf
                .set_stringn(x, y, text, usize::from(end.saturating_sub(x)), style)
                .0;
        }
    }
}

/// The bar color of a run of `commit`, its commit's age color, or the
/// uncommitted color for an uncommitted run.
fn run_color(lines: &BlameLines<'_>, commit: Option<u32>, colors: &BlameColors) -> [u8; 3] {
    commit
        .and_then(|index| lines.base?.ages.get(index as usize))
        .map_or(colors.uncommitted, |&bucket| {
            colors.ages[usize::from(bucket.min(3))]
        })
}

/// The texts of a run's label in order, each with its color and whether it
/// slants, within `room` characters at one gap between two texts.
///
/// A commit reads its short sha, the first word of its author, and its date,
/// in the colors of the line's `palette`. The author takes the room that the
/// sha, the date, and the gaps leave, cut to it, and goes with no room. An
/// uncommitted run reads `uncommitted` in italic.
fn label_parts(
    lines: &BlameLines<'_>,
    commit: Option<u32>,
    palette: &LinePalette,
    colors: &BlameColors,
    room: usize,
) -> Vec<(String, [u8; 3], bool)> {
    let Some(index) = commit else {
        return vec![("uncommitted".to_owned(), colors.uncommitted, true)];
    };
    let Some(entry) = lines
        .base
        .and_then(|base| base.blame.commits.get(index as usize))
    else {
        return Vec::new();
    };

    let sha = entry.info.short_sha.clone();
    let date = blame::short_date(entry.info.time, entry.utc_offset_minutes, lines.now_epoch);
    let used = sha.chars().count() + date.chars().count() + 2;
    let author: String = blame::author_word(&entry.info.author_name)
        .chars()
        .take(room.saturating_sub(used))
        .collect();

    let mut parts = vec![(sha, palette.bar, false)];
    if !author.is_empty() {
        parts.push((author, palette.author, false));
    }
    parts.push((date, palette.date, false));
    parts
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::{
        blame_cells, paint_blame_column, BlameColors, BlameLines, BLAME_COLUMN_CELLS,
        BLAME_MIN_TEXT_CELLS,
    };
    use crate::{
        app::DEFAULT_KEYMAP,
        blame::{BlameBase, BlameRows},
        host::{BlameCommit, CommitInfo, FileBlame},
        theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command};

    /// 2026-11-11T14:02:00Z, in unix epoch seconds.
    const NOV_11: i64 = 1_794_405_720;
    const DAY: i64 = 86_400;

    const COLORS: BlameColors = BlameColors {
        surface: [1, 1, 1],
        ages: [[10, 0, 0], [11, 0, 0], [12, 0, 0], [13, 0, 0]],
        divider: [2, 2, 2],
        author: [3, 3, 3],
        date: [4, 4, 4],
        uncommitted: [5, 5, 5],
        selected: [20, 20, 20],
        selected_surface: [22, 22, 22],
        selected_author: [24, 24, 24],
        selected_date: [26, 26, 26],
        scale: 256,
    };

    /// The ermine theme fills the column with its own surface, and
    /// default_dark, which sets no column or raised surface, with its
    /// background.
    #[test]
    fn the_surface_falls_back_to_the_raised_surface_then_the_background() {
        let surface = |name| shipped_colors(name).map(|colors| colors.surface);
        assert_eq!(
            [surface("ermine_night"), surface("default_dark")],
            [Some([0x14, 0x16, 0x1a]), Some([0x28, 0x2c, 0x34])],
        );
    }

    /// The ermine theme selects in its own bar, surface, and date colors, and
    /// the selected author takes `ui.text`. default_dark sets no selection
    /// scope, so it selects in its `ui.text` foreground on its `ui.selection`
    /// background.
    #[test]
    fn the_selection_falls_back_to_the_text_and_the_selection_scope() {
        let selection = |name| {
            shipped_colors(name).map(|colors| {
                [
                    colors.selected,
                    colors.selected_surface,
                    colors.selected_author,
                    colors.selected_date,
                ]
            })
        };
        let (ermine_text, dark_text) = ([0xdf, 0xe2, 0xe8], [0xfa, 0xfa, 0xfa]);
        assert_eq!(
            [selection("ermine_night"), selection("default_dark")],
            [
                Some([
                    [0x7f, 0xb0, 0xff],
                    [0x1b, 0x22, 0x30],
                    ermine_text,
                    [0x8f, 0xd1, 0x8a]
                ]),
                Some([dark_text, [0x56, 0xb6, 0xc2], dark_text, dark_text]),
            ],
            "the bar, the surface, the author, and the date of each theme",
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

    /// Every line takes a bar in its run's color and the surface beside it, a
    /// run's first line past line 0 takes a divider over its top, and each
    /// run's first line takes its label. At the scale of a whole cell the
    /// label holds 15 characters, so the author keeps one.
    #[test]
    fn each_run_draws_its_bar_its_divider_and_its_label() {
        let (rows, base) = three_runs();
        assert_eq!(
            scene_draws(&lines(&rows, &base, None)),
            three_runs_draws([13, 0, 0], [1, 1, 1], [3, 3, 3], [4, 4, 4]),
        );
    }

    /// The lines of the selected commit take the selection's bar and surface,
    /// and its label takes the selection's sha, author, and date colors. The
    /// other runs and the dividers keep their colors.
    #[test]
    fn the_selected_commits_lines_take_the_selection_colors() {
        let (rows, base) = three_runs();
        assert_eq!(
            scene_draws(&lines(&rows, &base, Some(1))),
            three_runs_draws([20, 20, 20], [22, 22, 22], [24, 24, 24], [26, 26, 26]),
        );
    }

    /// Without a live scene the column fills its cells but the last with the
    /// surface, and a run's first line writes its short sha and the first word
    /// of its author from the second cell, cut at the seventeenth, or
    /// `uncommitted` in italic.
    #[test]
    fn the_cell_fallback_writes_each_runs_sha_and_author() {
        let (rows, base) = three_runs();
        let lines = lines(&rows, &base, None);
        let area = Rect::new(0, 0, 18, 7);
        let mut buf = Buffer::empty(area);
        paint_blame_column(Some(&lines), &FOLDED, area, &COLORS, &mut buf, None);

        let row = |y: u16| (0..18).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(
            (
                [row(0), row(1), row(4), row(6)],
                buf[(1, 6)].modifier.contains(Modifier::ITALIC),
                [buf[(16, 3)].bg, buf[(17, 3)].bg],
            ),
            (
                [
                    " aaaaaaa Ada      ".to_owned(),
                    " ".repeat(18),
                    " bbbbbbb Barthol  ".to_owned(),
                    " uncommitted      ".to_owned(),
                ],
                true,
                [Color::Rgb(1, 1, 1), Color::Reset],
            ),
            "the labels, the italic run, and the surface ending a cell short",
        );
    }

    /// Without a live scene the selected commit's lines fill their cells with
    /// the selected surface, and its label writes in the selection's sha and
    /// author colors.
    #[test]
    fn the_cell_fallback_paints_the_selected_commit_in_its_colors() {
        let (rows, base) = three_runs();
        let lines = lines(&rows, &base, Some(1));
        let area = Rect::new(0, 0, 18, 7);
        let mut buf = Buffer::empty(area);
        paint_blame_column(Some(&lines), &FOLDED, area, &COLORS, &mut buf, None);

        let colors: Vec<[Color; 3]> = (0..7)
            .map(|y| [buf[(0, y)].bg, buf[(1, y)].fg, buf[(9, y)].fg])
            .collect();
        let (plain, selected, none) = (Color::Rgb(1, 1, 1), Color::Rgb(22, 22, 22), Color::Reset);
        assert_eq!(
            colors,
            [
                [plain, Color::Rgb(10, 0, 0), Color::Rgb(3, 3, 3)],
                [plain, none, none],
                [plain, none, none],
                [plain, none, none],
                [selected, Color::Rgb(20, 20, 20), Color::Rgb(24, 24, 24)],
                [selected, none, none],
                [plain, Color::Rgb(5, 5, 5), Color::Rgb(5, 5, 5)],
            ],
            "each row's surface, sha color, and author color",
        );
    }

    /// A label too large for the room keeps the parts that fit. At a scale of
    /// 400 the room holds 10 characters, so the sha alone fits.
    #[test]
    fn a_label_keeps_the_parts_that_fit_its_room() {
        let (rows, base) = three_runs();
        let lines = lines(&rows, &base, None);
        let area = Rect::new(0, 0, 18, 7);
        let mut scene = ApcScene::new();
        let colors = BlameColors {
            scale: 400,
            ..COLORS
        };
        paint_blame_column(
            Some(&lines),
            &FOLDED[..1],
            area,
            &colors,
            &mut Buffer::empty(area),
            Some(&mut scene),
        );

        let texts: Vec<String> = decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some(run.text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["aaaaaaa"]);
    }

    /// Dimming blends every color toward the background and keeps the scale.
    #[test]
    fn dimming_blends_every_color_and_keeps_the_scale() {
        let dimmed = COLORS.dim([0, 0, 0], 0.5);
        assert_eq!(
            (
                [
                    dimmed.surface,
                    dimmed.divider,
                    dimmed.author,
                    dimmed.date,
                    dimmed.uncommitted
                ],
                [
                    dimmed.selected,
                    dimmed.selected_surface,
                    dimmed.selected_author,
                    dimmed.selected_date,
                ],
                dimmed.ages,
                dimmed.scale,
            ),
            (
                [[1, 1, 1], [1, 1, 1], [2, 2, 2], [2, 2, 2], [3, 3, 3]],
                [[10, 10, 10], [11, 11, 11], [12, 12, 12], [13, 13, 13]],
                [[5, 0, 0], [6, 0, 0], [6, 0, 0], [7, 0, 0]],
                256,
            ),
        );
    }

    /// A decoded bar as its place, its size, and its color.
    type BarDraw = (i16, i16, u16, u16, [u8; 3]);

    /// A decoded text run as its text, its place, its color, and whether it
    /// slants.
    type RunDraw = (String, i16, i16, [u8; 3], bool);

    /// Lines 1 to 6 of a file, the second wrapped over two rows.
    const FOLDED: [(u32, u16); 6] = [(1, 1), (2, 2), (3, 1), (4, 1), (5, 1), (6, 1)];

    /// The column's colors in the shipped theme `name`.
    fn shipped_colors(name: &str) -> Option<BlameColors> {
        let config = stoat_config::parse(DEFAULT_KEYMAP)
            .0
            .expect("the shipped config parses");
        let theme = Theme::from_config(&config, name).expect("the shipped theme loads");
        BlameColors::resolve(&theme)
    }

    /// The blame of [`three_runs`] a day after its new commit, selecting
    /// `selected`.
    fn lines<'a>(
        rows: &'a BlameRows,
        base: &'a BlameBase,
        selected: Option<u32>,
    ) -> BlameLines<'a> {
        BlameLines {
            rows,
            base: Some(base),
            now_epoch: NOV_11 + DAY,
            selected,
        }
    }

    /// The bars and the text runs that `lines` paints over [`FOLDED`] in a
    /// live scene, in [`COLORS`], in place order.
    fn scene_draws(lines: &BlameLines<'_>) -> (Vec<BarDraw>, Vec<RunDraw>) {
        let area = Rect::new(0, 0, 18, 7);
        let mut scene = ApcScene::new();
        paint_blame_column(
            Some(lines),
            &FOLDED,
            area,
            &COLORS,
            &mut Buffer::empty(area),
            Some(&mut scene),
        );

        let (mut bars, mut texts) = (Vec::new(), Vec::new());
        for command in decode_stream(scene.buffer()) {
            match command {
                Command::Bar(bar) => bars.push((bar.x, bar.y, bar.width, bar.height, bar.color)),
                Command::TextRun(run) => {
                    texts.push((run.text, run.col, run.row, run.color, run.italic))
                },
                _ => {},
            }
        }
        bars.sort_by_key(|&(x, y, ..)| (y, x));
        texts.sort_by_key(|&(_, x, y, ..)| (y, x));
        (bars, texts)
    }

    /// What [`scene_draws`] answers for [`three_runs`] in [`COLORS`], with the
    /// new commit's lines in `bar`, `surface`, `author`, and `date`.
    ///
    /// The old commit's run keeps its age color, the plain surface, and the
    /// plain label, and the uncommitted run keeps its own color.
    fn three_runs_draws(
        bar: [u8; 3],
        surface: [u8; 3],
        author: [u8; 3],
        date: [u8; 3],
    ) -> (Vec<BarDraw>, Vec<RunDraw>) {
        let (old, plain, divider) = ([10, 0, 0], [1, 1, 1], [2, 2, 2]);
        let text = |text: &str, x, y, color, italic| (text.to_owned(), x, y, color, italic);
        (
            vec![
                (0, 0, 4, 16, old),
                (4, 0, 277, 16, plain),
                (0, 16, 4, 32, old),
                (4, 16, 277, 32, plain),
                (0, 48, 4, 16, old),
                (4, 48, 277, 16, plain),
                (0, 64, 4, 16, bar),
                (4, 64, 277, 1, divider),
                (4, 65, 277, 15, surface),
                (0, 80, 4, 16, bar),
                (4, 80, 277, 16, surface),
                (0, 96, 4, 16, [5, 5, 5]),
                (4, 96, 277, 1, divider),
                (4, 97, 277, 15, plain),
            ],
            vec![
                text("aaaaaaa", 14, 0, old, false),
                text("A", 142, 0, [3, 3, 3], false),
                text("11/01", 174, 0, [4, 4, 4], false),
                text("bbbbbbb", 14, 64, bar, false),
                text("B", 142, 64, author, false),
                text("11/11", 174, 64, date, false),
                text("uncommitted", 14, 96, [5, 5, 5], true),
            ],
        )
    }

    /// Rows 0 to 2 of an old commit, rows 3 and 4 of a new one, and an
    /// uncommitted row 5, with the two commits in age buckets 0 and 3.
    fn three_runs() -> (BlameRows, BlameBase) {
        let commit = |short_sha: &str, author: &str, time| BlameCommit {
            info: CommitInfo {
                sha: short_sha.to_owned(),
                short_sha: short_sha.to_owned(),
                summary: String::new(),
                author_name: author.to_owned(),
                author_email: String::new(),
                time,
                parents: Vec::new(),
            },
            utc_offset_minutes: 0,
        };
        let rows = BlameRows {
            runs: vec![(0..3, Some(0)), (3..5, Some(1)), (5..6, None)],
        };
        let base = BlameBase {
            text: String::new(),
            blame: FileBlame {
                runs: Vec::new(),
                commits: vec![
                    commit("aaaaaaa", "Ada Lovelace", NOV_11 - 10 * DAY),
                    commit("bbbbbbb", "Bartholomew Smith", NOV_11),
                ],
            },
            ages: vec![0, 3],
        };
        (rows, base)
    }
}
