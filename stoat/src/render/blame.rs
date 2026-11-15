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
    pub(crate) fn resolve(theme: &Theme) -> Option<BlameColors> {
        let fg = |name: &str| style_rgb(theme.try_get(name)?.fg);
        let surface = [
            scope::UI_BLAME_COLUMN,
            scope::UI_SURFACE_RAISED,
            scope::UI_BACKGROUND,
        ]
        .into_iter()
        .find_map(|name| style_rgb(theme.try_get(name)?.bg))?;
        let muted = fg(scope::UI_TEXT_MUTED)?;

        Some(BlameColors {
            surface,
            ages: scope::UI_BLAME_AGE.map(|name| fg(name).unwrap_or(muted)),
            divider: fg(scope::UI_DIVIDER)?,
            author: fg(scope::UI_TEXT_DIM)?,
            date: muted,
            uncommitted: muted,
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
            scale: self.scale,
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
        let color = run_color(lines, *commit, colors);
        let surface_width = BLAME_SURFACE_WIDTH - RUN_BAR_WIDTH;
        bars.push(area, bar(0, y, RUN_BAR_WIDTH, span, color));
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
                colors.surface,
            ),
        );

        if starts {
            let room = usize::from(LABEL_END - LABEL_START) * 16 / usize::from(colors.scale.max(1));
            let mut chars = 0;
            for (text, color, italic) in label_parts(lines, *commit, color, colors, room) {
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
fn paint_cells(
    lines: Option<&BlameLines<'_>>,
    folded: &[(u32, u16)],
    area: Rect,
    colors: &BlameColors,
    buf: &mut Buffer,
) {
    let rows: u16 = folded.iter().map(|&(_, height)| height).sum();
    let surface = rgb(colors.surface);
    let cells = Rect {
        width: area.width.min(BLAME_COLUMN_CELLS - 1),
        height: rows.min(area.height),
        ..area
    }
    .intersection(buf.area);
    for y in cells.top()..cells.bottom() {
        for x in cells.left()..cells.right() {
            buf[(x, y)].set_bg(surface);
        }
    }

    let Some(lines) = lines else {
        return;
    };
    let mut top = 0u16;
    for &(number, height) in folded {
        let (y, line) = (area.y + top, number.saturating_sub(1));
        top += height;
        let Some((_, commit)) = lines.rows.run(line).filter(|(rows, _)| rows.start == line) else {
            continue;
        };
        if !cells.contains((cells.x, y).into()) {
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
                let author = style(colors.author);
                vec![
                    (
                        entry.info.short_sha.as_str(),
                        style(run_color(lines, *commit, colors)),
                    ),
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
/// A commit reads its short sha in the run's `color`, the first word of its
/// author, and its date. The author takes the room that the sha, the date,
/// and the gaps leave, cut to it, and goes with no room. An uncommitted run
/// reads `uncommitted` in italic.
fn label_parts(
    lines: &BlameLines<'_>,
    commit: Option<u32>,
    color: [u8; 3],
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

    let mut parts = vec![(sha, color, false)];
    if !author.is_empty() {
        parts.push((author, colors.author, false));
    }
    parts.push((date, colors.date, false));
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
        scale: 256,
    };

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

    /// Every line takes a bar in its run's color and the surface beside it, a
    /// run's first line past line 0 takes a divider over its top, and each
    /// run's first line takes its label. At the scale of a whole cell the
    /// label holds 15 characters, so the author keeps one.
    #[test]
    fn each_run_draws_its_bar_its_divider_and_its_label() {
        let (rows, base) = three_runs();
        let lines = BlameLines {
            rows: &rows,
            base: Some(&base),
            now_epoch: NOV_11 + DAY,
        };
        let area = Rect::new(0, 0, 18, 7);
        let mut scene = ApcScene::new();
        paint_blame_column(
            Some(&lines),
            &FOLDED,
            area,
            &COLORS,
            &mut Buffer::empty(area),
            Some(&mut scene),
        );

        let mut bars = Vec::new();
        let mut texts = Vec::new();
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

        let (old, new, surface, divider) = ([10, 0, 0], [13, 0, 0], [1, 1, 1], [2, 2, 2]);
        let text = |text: &str, x, y, color, italic| (text.to_owned(), x, y, color, italic);
        assert_eq!(
            (bars, texts),
            (
                vec![
                    (0, 0, 4, 16, old),
                    (4, 0, 277, 16, surface),
                    (0, 16, 4, 32, old),
                    (4, 16, 277, 32, surface),
                    (0, 48, 4, 16, old),
                    (4, 48, 277, 16, surface),
                    (0, 64, 4, 16, new),
                    (4, 64, 277, 1, divider),
                    (4, 65, 277, 15, surface),
                    (0, 80, 4, 16, new),
                    (4, 80, 277, 16, surface),
                    (0, 96, 4, 16, [5, 5, 5]),
                    (4, 96, 277, 1, divider),
                    (4, 97, 277, 15, surface),
                ],
                vec![
                    text("aaaaaaa", 14, 0, old, false),
                    text("A", 142, 0, [3, 3, 3], false),
                    text("11/01", 174, 0, [4, 4, 4], false),
                    text("bbbbbbb", 14, 64, new, false),
                    text("B", 142, 64, [3, 3, 3], false),
                    text("11/11", 174, 64, [4, 4, 4], false),
                    text("uncommitted", 14, 96, [5, 5, 5], true),
                ],
            ),
        );
    }

    /// Without a live scene the column fills its cells but the last with the
    /// surface, and a run's first line writes its short sha and the first word
    /// of its author from the second cell, cut at the seventeenth, or
    /// `uncommitted` in italic.
    #[test]
    fn the_cell_fallback_writes_each_runs_sha_and_author() {
        let (rows, base) = three_runs();
        let lines = BlameLines {
            rows: &rows,
            base: Some(&base),
            now_epoch: NOV_11 + DAY,
        };
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

    /// A label too large for the room keeps the parts that fit. At a scale of
    /// 400 the room holds 10 characters, so the sha alone fits.
    #[test]
    fn a_label_keeps_the_parts_that_fit_its_room() {
        let (rows, base) = three_runs();
        let lines = BlameLines {
            rows: &rows,
            base: Some(&base),
            now_epoch: NOV_11 + DAY,
        };
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
                dimmed.ages,
                dimmed.scale,
            ),
            (
                [[1, 1, 1], [1, 1, 1], [2, 2, 2], [2, 2, 2], [3, 3, 3]],
                [[5, 0, 0], [6, 0, 0], [6, 0, 0], [7, 0, 0]],
                256,
            ),
        );
    }

    /// Lines 1 to 6 of a file, the second wrapped over two rows.
    const FOLDED: [(u32, u16); 6] = [(1, 1), (2, 2), (3, 1), (4, 1), (5, 1), (6, 1)];

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
