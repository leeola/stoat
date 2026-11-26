//! Shared painters for a [`crate::picker::PathPicker`]'s result list and
//! preview.
//!
//! The standalone file finder and the palette's inline argument picker paint
//! the same list of repo-relative rows beside the same preview pane. Both drive
//! a `PathPicker`, so both render through these functions and cannot drift.

use crate::{
    display_map::DisplaySnapshot,
    editor_state::EditorState,
    fuzzy,
    input_view::InputView,
    picker::{write_row_display, PickList, Preview, RowTops, ScaledPreview},
    render::{
        chrome::{PlateColors, SELECTED_ROW_RADIUS},
        editor::{self, render_editor, ResolvedChrome},
        paint::style_rgb,
        pixel_rows::{self, PixelBar, Placer, RowMetrics, RunsAt, ROW_PAD},
        text::{clip_to_width, write_str, write_str_clipped},
        TypeScale,
    },
    theme::{scope, Theme, TypeRole, FULL_CELL_SCALE},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::{
    collections::BTreeMap,
    fmt::Write,
    path::{Path, PathBuf},
};
use stoat_text::Point;
use stoat_widgets::{
    bar::Bar,
    pool,
    text_run::{self, TextRun, TextRunBatch},
};
use unicode_width::UnicodeWidthStr;

/// The glyph before a filter modal's query.
pub(crate) const FILTER_PROMPT: &str = "\u{203A}";

/// Columns the query keeps on a filter modal's header row. A row too narrow
/// for the query and the words right of it drops the chip first, then the
/// count.
pub(super) const MIN_QUERY_COLUMNS: u16 = 12;

/// The logical px a plate's header row draws right and down of its cells.
///
/// The plate's side stands 11 px outside the row, so 1 px right sets the row's
/// text 12 px in from the side, as the boards pad the header. 7 px down centers
/// an 18 px row in the boards' 32 px band.
pub(super) const PLATE_HEADER_NUDGE: [i16; 2] = [1, 7];

/// Sixteenths between two parts of a plate's header row, the boards' 8 px gap
/// at a 9 px cell.
pub(super) const PLATE_HEADER_GAP: u16 = 14;

/// Sixteenths on each side of a chip's text, the boards' 6 px padding at a 9 px
/// cell.
///
/// Every chip takes it, boxed or not, so a pill keeps its place when the scope
/// changes.
const CHIP_PAD: u16 = 11;

/// Sixteenths between two chips of a plate's header row, the boards' 2 px gap
/// at a 9 px cell.
const PILL_GAP: u16 = 4;

/// Sixteenths that a plate header's row ends inside its inner rect, 2 px, so
/// under the 1 px of [`PLATE_HEADER_NUDGE`] it ends 12 px inside the plate.
const PLATE_END_INSET: u16 = 4;

/// Sixteenths that a plate's block caret stands under its row's top, the
/// boards' 1 px of an 18 px row.
const CARET_TOP: i16 = 1;

/// Sixteenths a plate's block caret is high. Under [`CARET_TOP`] it covers the
/// pixel rows 1 to 15 of an 18 px row, the boards' 15 px caret, which leaves
/// 2 px of the row under it.
const CARET_HEIGHT: u16 = 13;

/// Rows a modal's key line takes, the inner rect's last row and the bottom
/// border's row under it.
///
/// Every filter modal ends in this one shape in both looks, so no layout reads
/// the look.
pub(crate) const FOOTER_ROWS: u16 = 2;

/// The logical px a plate's key line draws right and down of its row.
///
/// 1 px right sets the line's text 12 px in from the plate's side, as the
/// header's. The line spans the 24 px from 4 px under the row's top, where the
/// Islands end, to the plate's bottom, so 7 px down centers an 18 px row in it.
const PLATE_FOOTER_NUDGE: [i16; 2] = [1, 7];

/// Sixteenths between two hints of a plate's key line, the boards' 14 px at a
/// 9 px cell.
const PLATE_HINT_GAP: u16 = 25;

/// Sixteenths between the hints, the legend, and the note of a plate's key
/// line, the boards' 10 px at a 9 px cell.
const PLATE_FOOTER_GAP: u16 = 18;

/// The key line of a list whose selection opens and shows in a preview, the
/// symbol finder's and the target pickers'.
pub(crate) const PICKER_HINTS: [(&str, &str); 3] = [
    ("Enter", "open"),
    ("C-d/C-u", "preview"),
    ("C-f/C-b", "page"),
];

/// The words a filter modal's header row shows around its query.
pub(crate) struct FilterHeader<'a> {
    /// The modal's name, painted first.
    pub(crate) label: &'a str,
    /// The glyph before the query, [`FILTER_PROMPT`] in every filter modal.
    pub(crate) prompt: &'a str,
    /// How many entries the query keeps, and how many the list holds.
    pub(crate) count: Option<(usize, usize)>,
    /// The notes at the row's right end, left to right, each with whether it
    /// is the active one.
    ///
    /// A scope switch names every scope and marks the one the list shows. A
    /// lone active note names the key that switches the scope.
    pub(crate) chips: &'a [(&'a str, bool)],
}

/// Paint a filter modal's header, which is the label, the prompt glyph, the
/// live input, the count and chips right of it, and the separator ruling the
/// row off from the body.
///
/// `inner` is the modal's rect inside its border. The header claims its first
/// two rows, so a caller's body starts at `inner.y + 2`.
///
/// The input starts one column past the prompt, so a longer label or prompt
/// pushes the input right.
///
/// Under the plate look of [`plate_colors`](crate::render::chrome::plate_colors),
/// the first row draws as runs in a 32 px band on the plate's ground, and no
/// rule draws under it. The input's editor then paints no cells, and the query
/// shows the input's text with a block caret.
///
/// Every filter modal opens this way, and each lays its body rects out from
/// that same two-row assumption. One painter is what keeps the assumption true
/// across all of them at once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_header(
    buf: &mut Buffer,
    inner: Rect,
    header: &FilterHeader<'_>,
    input: &InputView,
    ws: &mut Workspace,
    theme: &Theme,
    chrome: &ResolvedChrome,
    scene: &mut stoat_widgets::ApcScene,
) {
    if let Some(plate) = crate::render::chrome::plate_colors(theme, scene) {
        paint_plate_header(
            buf,
            inner,
            header,
            input.caret(ws),
            plate.ground,
            theme,
            scene,
        );
        return;
    }

    let input_area = paint_header_row(buf, inner, header, theme, scene);
    input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        chrome,
        &BTreeMap::new(),
        buf,
    );

    crate::render::chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        theme.get(scope::UI_BORDER_INACTIVE),
        scene,
    );
}

/// Paint the words of the header row into `inner`'s first row, and return the
/// rect left for the query.
///
/// The count and the chips sit flush with the row's right end, one column
/// apart, and one column clear of the query. An active chip's box goes through
/// [`chip_cells`](crate::render::chrome::chip_cells), so a live scene draws it
/// as a panel. An inactive chip is plain muted text on the row's background. A
/// row too narrow for all of them drops the chips together, then the count.
///
/// The count reads muted. A count of no shown entries paints its zero in
/// `diff.deleted`'s foreground, which flags a query that matches nothing.
fn paint_header_row(
    buf: &mut Buffer,
    inner: Rect,
    header: &FilterHeader<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) -> Rect {
    let (y, end) = (inner.y, inner.x + inner.width);

    let mut x = inner.x;
    if !header.label.is_empty() {
        write_str_clipped(buf, x, y, header.label, label_style(theme), end);
        x = x.saturating_add(header.label.width() as u16 + 1);
    }
    write_str_clipped(buf, x, y, header.prompt, theme.get(scope::UI_PROMPT), end);
    let query_x = x.saturating_add(header.prompt.width() as u16 + 1).min(end);

    let mut count = header
        .count
        .map(|(shown, total)| format!("{shown} of {total}"));
    let mut chips: Vec<(String, bool)> = header
        .chips
        .iter()
        .map(|&(chip, active)| (format!(" {chip} "), active))
        .collect();
    let fits = |count: &Option<String>, chips: &[(String, bool)]| {
        let trailing: u16 = count
            .iter()
            .chain(chips.iter().map(|(chip, _)| chip))
            .map(|part| part.width() as u16 + 1)
            .sum();
        query_x + MIN_QUERY_COLUMNS + trailing <= end
    };
    if !fits(&count, &chips) {
        chips.clear();
    }
    if !fits(&count, &chips) {
        count = None;
    }

    let mut right = end;
    for (chip, active) in chips.iter().rev() {
        let width = chip.width() as u16;
        right -= width;
        let style = match active {
            true => {
                let cells = Rect::new(right, y, width, 1);
                crate::render::chrome::chip_cells(cells, theme.get(scope::UI_CHIP), scene)
            },
            false => theme.get(scope::UI_TEXT_MUTED),
        };
        write_str(buf, right, y, chip, style);
        right -= 1;
    }
    if let Some(count) = count {
        right -= count.width() as u16;
        let muted = theme.get(scope::UI_TEXT_MUTED);
        write_str(buf, right, y, &count, muted);
        if let Some((0, _)) = header.count {
            let zero = match theme.get(scope::DIFF_DELETED).fg {
                Some(fg) => muted.fg(fg),
                None => muted,
            };
            write_str(buf, right, y, "0", zero);
        }
        right -= 1;
    }
    Rect::new(query_x, y, right.saturating_sub(query_x), 1)
}

/// The style of the modal's name on a header row, `ui.modal.label`, or the
/// bold `ui.text.dim` where a theme leaves it unset.
fn label_style(theme: &Theme) -> Style {
    theme
        .try_get(scope::UI_MODAL_LABEL)
        .unwrap_or_else(|| theme.get(scope::UI_TEXT_DIM).add_modifier(Modifier::BOLD))
}

/// Paint `inner`'s first row as a plate's header, runs in the boards' 32 px
/// band on the plate's `ground`.
///
/// The row's cells blank to the ground, and every part draws
/// [`PLATE_HEADER_NUDGE`] px off the cells. The label, the prompt glyph, and
/// the query start at the row's left, [`PLATE_HEADER_GAP`] apart, at the
/// [`TypeRole::Ui`] size. The count and the chips end at the row's right at
/// the [`TypeRole::Caption`] size, as [`plate_trailing`] places them.
///
/// The query is the input's text `query`, as [`query_window`] fits it around
/// `caret`, the char its cursor covers, and [`plate_caret`] draws the block
/// caret over that char in the text color. A label or a prompt that runs past
/// the row's end clips there. A count of no shown entries paints its zero in
/// `diff.deleted`'s foreground, as [`paint_header_row`] does. A part whose
/// color has no RGB value draws nothing.
fn paint_plate_header(
    buf: &mut Buffer,
    inner: Rect,
    header: &FilterHeader<'_>,
    (query, caret): (String, usize),
    ground: [u8; 3],
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    if inner.is_empty() {
        return;
    }
    blank_plate_row(buf, inner, ground);

    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let advance = |text: &str, scale| text_run::advance_sixteenths(text.chars().count(), scale);
    let chars_in = |room: u16| usize::from(room) * 16 / usize::from(ui.max(1));
    let label = label_style(theme);
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let text = style_rgb(theme.get(scope::UI_TEXT).fg);
    let chip = theme.get(scope::UI_CHIP);
    let fill = style_rgb(chip.bg);

    let end = inner.width.saturating_mul(16);
    let mut query_x: u16 = 0;
    if !header.label.is_empty() {
        query_x = advance(header.label, ui).saturating_add(PLATE_HEADER_GAP);
    }
    let prompt_x = query_x;
    query_x = query_x
        .saturating_add(advance(header.prompt, ui))
        .saturating_add(PLATE_HEADER_GAP);
    let label_text = clip_to_width(header.label, chars_in(end));
    let prompt_text = clip_to_width(header.prompt, chars_in(end.saturating_sub(prompt_x)));

    let count = header
        .count
        .map(|(shown, total)| format!("{shown} of {total}"));
    let trailing = plate_trailing(
        count.as_deref(),
        header.chips,
        fill.is_some(),
        end,
        query_x.saturating_add(MIN_QUERY_COLUMNS * 16),
        caption,
    );
    let room = chars_in(trailing.query_end.saturating_sub(query_x));
    let (before, at_caret, after) = query_window(&query, caret, room);

    scene.nudge(PLATE_HEADER_NUDGE);
    if let Some(fill) = fill {
        plate_chip_boxes(&trailing, fill, inner, buf, scene);
    }

    let origin = Rect::new(inner.x, inner.y, 1, 1);
    let mut run = |col: u16, part: &str, scale: u16, colors: (Option<[u8; 3]>, [u8; 3]), bold| {
        if let (Some(color), false) = (colors.0, part.is_empty()) {
            plate_run(col, part, scale, color, colors.1, bold).render(origin, buf, scene);
        }
    };
    let bold = |style: Style| style.add_modifier.contains(Modifier::BOLD);

    run(
        0,
        label_text,
        ui,
        (style_rgb(label.fg), ground),
        bold(label),
    );
    run(
        prompt_x,
        prompt_text,
        ui,
        (style_rgb(muted.fg), ground),
        bold(muted),
    );
    run(query_x, before, ui, (text, ground), false);
    let caret_x = query_x + text_run::advance_sixteenths(before.chars().count(), ui);
    let after_x = query_x + text_run::advance_sixteenths(before.chars().count() + 1, ui);
    run(after_x, after, ui, (text, ground), false);

    if let (Some(col), Some(count), Some((shown, _))) = (trailing.count, &count, header.count) {
        let (zero, rest) = count.split_at(usize::from(shown == 0));
        let deleted = theme.get(scope::DIFF_DELETED).fg.or(muted.fg);
        run(col, zero, caption, (style_rgb(deleted), ground), false);
        run(
            col + advance(zero, caption),
            rest,
            caption,
            (style_rgb(muted.fg), ground),
            false,
        );
    }
    for (&(name, active), (col, cells)) in header.chips.iter().zip(&trailing.chips) {
        let colors = match (active, cells) {
            (true, Some(_)) => (style_rgb(chip.fg), fill.unwrap_or(ground)),
            (true, None) => (style_rgb(chip.fg), ground),
            (false, _) => (style_rgb(muted.fg), ground),
        };
        run(*col, name, caption, colors, false);
    }
    if let (Some(at_caret), Some(text)) = (at_caret, text) {
        plate_caret(caret_x, at_caret, ui, (text, ground), origin, buf, scene);
    }

    scene.nudge([0, 0]);
}

/// Blank `row`'s cells to the plate's `ground`, under the runs of a plate's
/// header or key line.
pub(super) fn blank_plate_row(buf: &mut Buffer, row: Rect, ground: [u8; 3]) {
    let [r, g, b] = ground;
    for x in row.left()..row.right() {
        buf[(x, row.y)]
            .set_char(' ')
            .set_style(Style::default().bg(Color::Rgb(r, g, b)));
    }
}

/// Draw a rounded chip box in `fill` under each boxed chip that
/// [`plate_trailing`] placed on `inner`'s first row.
///
/// A box is a bar the height of the row in sixteenths, so it stands at the
/// boards' pixels rather than on cells. The boxes must follow the row's
/// [`PLATE_HEADER_NUDGE`], so they stand off the cells as the chips' runs do.
pub(super) fn plate_chip_boxes(
    trailing: &PlateTrailing,
    fill: [u8; 3],
    inner: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let row = Rect::new(inner.x, inner.y, inner.width, 1);
    for (_, chip_box) in &trailing.chips {
        if let Some((left, width)) = *chip_box {
            Bar {
                x: left as i16,
                y: 0,
                width,
                height: 16,
                color: fill,
                radius: 4,
                corners: 0,
            }
            .render(row, buf, scene);
        }
    }
}

/// The places of a plate header's count and chips, which end at its right.
#[derive(Debug, PartialEq)]
pub(super) struct PlateTrailing {
    /// The count's run start, in sixteenths from the row's left.
    pub(super) count: Option<u16>,
    /// Each kept chip's run start, and for a boxed chip the left and the width
    /// of its box, all in sixteenths from the row's left.
    pub(super) chips: Vec<(u16, Option<(u16, u16)>)>,
    /// The sixteenth the query ends by.
    pub(super) query_end: u16,
}

/// Place a plate header's `count` and `chips` right to left from
/// [`PLATE_END_INSET`] inside `end` sixteenths, at `scale`.
///
/// Each chip takes its text with [`CHIP_PAD`] on each side, and the chips
/// stand [`PILL_GAP`] apart. With `boxed`, an active chip's box is that span.
/// The count ends [`PLATE_HEADER_GAP`] left of the first chip, or where the
/// chips would end when there is none. The query ends [`PLATE_HEADER_GAP`]
/// left of what stands right of it.
///
/// If the query then ends left of `query_min`, the chips drop together. If it
/// still does, the count drops too, as [`paint_header_row`] drops them.
///
/// A [`PlateHeader`]'s meta takes the count's place, and its title the query's.
pub(super) fn plate_trailing(
    count: Option<&str>,
    chips: &[(&str, bool)],
    boxed: bool,
    end: u16,
    query_min: u16,
    scale: u16,
) -> PlateTrailing {
    let advance = |text: &str| text_run::advance_sixteenths(text.chars().count(), scale);
    let place = |count: Option<&str>, chips: &[(&str, bool)]| {
        let mut right = end.saturating_sub(PLATE_END_INSET);
        let mut first_left = None;
        let mut placed = Vec::with_capacity(chips.len());
        for &(chip, active) in chips.iter().rev() {
            let width = advance(chip) + 2 * CHIP_PAD;
            let left = right.saturating_sub(width);
            let chip_box = (active && boxed).then_some((left, width));
            placed.push((left + CHIP_PAD, chip_box));
            first_left = Some(left);
            right = left.saturating_sub(PILL_GAP);
        }
        placed.reverse();

        if let Some(left) = first_left {
            right = left.saturating_sub(PLATE_HEADER_GAP);
        }
        let count = count.map(|count| {
            let col = right.saturating_sub(advance(count));
            right = col.saturating_sub(PLATE_HEADER_GAP);
            col
        });
        PlateTrailing {
            count,
            chips: placed,
            query_end: right,
        }
    };

    let all = place(count, chips);
    if all.query_end >= query_min {
        return all;
    }
    let counted = place(count, &[]);
    if counted.query_end >= query_min {
        return counted;
    }
    place(None, &[])
}

/// The chars of `query` that a plate header shows in `room` chars, in three
/// parts around the caret on char `caret`.
///
/// The parts are the chars before the caret, the caret's own char, and the
/// chars after it. A caret past the query's end covers a space. The window keeps the query's
/// tail, but it starts at the caret when the caret stands left of that tail,
/// so the caret stays in view. A window too narrow for the caret shows nothing.
fn query_window(query: &str, caret: usize, room: usize) -> (&str, Option<&str>, &str) {
    let len = query.chars().count();
    let caret = caret.min(len);
    let start = (len + 1).saturating_sub(room).min(caret);
    let stop = (start + room).min(len + 1);
    if caret >= stop {
        return ("", None, "");
    }

    let byte = |index: usize| {
        query
            .char_indices()
            .nth(index)
            .map_or(query.len(), |(at, _)| at)
    };
    let at_caret = match caret < len {
        true => &query[byte(caret)..byte(caret + 1)],
        false => " ",
    };
    (
        &query[byte(start)..byte(caret)],
        Some(at_caret),
        &query[byte((caret + 1).min(len))..byte(stop.min(len))],
    )
}

/// The words of a header row that names one item, such as the commit that a
/// preview shows, where a filter modal's row holds its query.
pub(crate) struct PlateHeader<'a> {
    /// The modal's name, painted first.
    pub(crate) label: &'a str,
    /// The item's name, bold after the label.
    pub(crate) title: &'a str,
    /// Styled spans that end left of the chip, each in its own color.
    pub(crate) meta: &'a [(String, Style)],
    /// A note that ends the row, boxed in `ui.preview.chip`.
    pub(crate) chip: Option<&'a str>,
}

/// Paint a header that names one item, which is the label, the bold title, and
/// the meta and the chip at the row's right end.
///
/// `inner` is the modal's rect inside its border. The header claims its first
/// two rows, as [`filter_header`] does, so a caller's body starts at
/// `inner.y + 2`.
///
/// The title keeps [`MIN_QUERY_COLUMNS`], as a filter modal's query does. A row
/// too narrow for the title and the parts right of it drops the chip first,
/// then the meta. The title clips where the kept parts start. A theme with no
/// `ui.preview.chip` style draws the chip's text plain in `ui.text.muted`.
///
/// Under the plate look of [`plate_colors`](crate::render::chrome::plate_colors),
/// the first row draws as runs in a 32 px band on the plate's ground, as
/// [`filter_header`] draws its row there, and no rule draws under it.
/// Otherwise the parts draw in the row's cells, and a rule in
/// `ui.border.inactive` rules the row off from the body.
pub(crate) fn plate_header(
    buf: &mut Buffer,
    inner: Rect,
    header: &PlateHeader<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    if let Some(plate) = crate::render::chrome::plate_colors(theme, scene) {
        paint_plate_title_row(buf, inner, header, plate.ground, theme, scene);
        return;
    }

    paint_title_row(buf, inner, header, theme, scene);
    crate::render::chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        theme.get(scope::UI_BORDER_INACTIVE),
        scene,
    );
}

/// Paint `header`'s parts into `inner`'s first row as cells, for
/// [`plate_header`].
///
/// The meta and the chip sit flush with the row's right end, one column apart,
/// and one column clear of the title, as [`paint_header_row`] sets a filter
/// modal's count and chips. The chip's box goes through
/// [`chip_cells`](crate::render::chrome::chip_cells), so a live scene draws it
/// as a panel.
fn paint_title_row(
    buf: &mut Buffer,
    inner: Rect,
    header: &PlateHeader<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let (y, end) = (inner.y, inner.x + inner.width);

    let mut title_x = inner.x;
    if !header.label.is_empty() {
        write_str_clipped(buf, title_x, y, header.label, label_style(theme), end);
        title_x = title_x.saturating_add(header.label.width() as u16 + 1);
    }

    let meta_width: u16 = header
        .meta
        .iter()
        .map(|(text, _)| text.width() as u16)
        .sum();
    let mut meta = (!header.meta.is_empty()).then_some(meta_width);
    let mut chip = header.chip.map(|chip| format!(" {chip} "));
    let fits = |meta: Option<u16>, chip: &Option<String>| {
        let trailing = meta.map_or(0, |width| width + 1)
            + chip.as_ref().map_or(0, |chip| chip.width() as u16 + 1);
        title_x + MIN_QUERY_COLUMNS + trailing <= end
    };
    if !fits(meta, &chip) {
        chip = None;
    }
    if !fits(meta, &chip) {
        meta = None;
    }

    let mut right = end;
    if let Some(chip) = &chip {
        let width = chip.width() as u16;
        right -= width;
        let cells = Rect::new(right, y, width, 1);
        let style = match theme.try_get(scope::UI_PREVIEW_CHIP) {
            Some(style) => crate::render::chrome::chip_cells(cells, style, scene),
            None => theme.get(scope::UI_TEXT_MUTED),
        };
        write_str(buf, right, y, chip, style);
        right -= 1;
    }
    if let Some(width) = meta {
        right -= width;
        let mut x = right;
        for (text, style) in header.meta {
            write_str(buf, x, y, text, *style);
            x += text.width() as u16;
        }
        right -= 1;
    }

    let title = theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD);
    write_str_clipped(buf, title_x, y, header.title, title, right);
}

/// Paint `header`'s parts into `inner`'s first row as a plate's header, runs
/// in the boards' 32 px band on the plate's `ground`, for [`plate_header`].
///
/// The row's cells blank to the ground, and every part draws
/// [`PLATE_HEADER_NUDGE`] px off the cells. The label and the title start at
/// the row's left, [`PLATE_HEADER_GAP`] apart, at the [`TypeRole::Ui`] size.
/// The meta and the chip end at the row's right at the [`TypeRole::Caption`]
/// size, as [`plate_trailing`] places them. A part whose color has no RGB value
/// draws nothing.
fn paint_plate_title_row(
    buf: &mut Buffer,
    inner: Rect,
    header: &PlateHeader<'_>,
    ground: [u8; 3],
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    if inner.is_empty() {
        return;
    }
    blank_plate_row(buf, inner, ground);

    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let advance = |text: &str, scale| text_run::advance_sixteenths(text.chars().count(), scale);
    let chars_in = |room: u16| usize::from(room) * 16 / usize::from(ui.max(1));
    let label = label_style(theme);
    let chip = theme.try_get(scope::UI_PREVIEW_CHIP);
    let fill = chip.and_then(|style| style_rgb(style.bg));

    let end = inner.width.saturating_mul(16);
    let title_x = match header.label.is_empty() {
        true => 0,
        false => advance(header.label, ui).saturating_add(PLATE_HEADER_GAP),
    };
    let meta: String = header.meta.iter().map(|(text, _)| text.as_str()).collect();
    let chips: Vec<(&str, bool)> = header.chip.map(|chip| (chip, true)).into_iter().collect();
    let trailing = plate_trailing(
        Some(meta.as_str()).filter(|meta| !meta.is_empty()),
        &chips,
        fill.is_some(),
        end,
        title_x.saturating_add(MIN_QUERY_COLUMNS * 16),
        caption,
    );
    let label_text = clip_to_width(header.label, chars_in(end));
    let title_room = chars_in(trailing.query_end.saturating_sub(title_x));
    let title_text = clip_to_width(header.title, title_room);

    scene.nudge(PLATE_HEADER_NUDGE);
    if let Some(fill) = fill {
        plate_chip_boxes(&trailing, fill, inner, buf, scene);
    }

    let origin = Rect::new(inner.x, inner.y, 1, 1);
    let mut run = |col: u16, part: &str, scale: u16, colors: (Option<[u8; 3]>, [u8; 3]), bold| {
        if let (Some(color), false) = (colors.0, part.is_empty()) {
            plate_run(col, part, scale, color, colors.1, bold).render(origin, buf, scene);
        }
    };
    let bold = |style: Style| style.add_modifier.contains(Modifier::BOLD);
    let text = style_rgb(theme.get(scope::UI_TEXT).fg);

    run(
        0,
        label_text,
        ui,
        (style_rgb(label.fg), ground),
        bold(label),
    );
    run(title_x, title_text, ui, (text, ground), true);
    if let Some(mut col) = trailing.count {
        for (part, style) in header.meta {
            let colors = (style_rgb(style.fg), ground);
            run(col, part.as_str(), caption, colors, bold(*style));
            col += advance(part, caption);
        }
    }
    for (&(name, _), (col, cells)) in chips.iter().zip(&trailing.chips) {
        let colors = match (chip, cells) {
            (Some(style), Some(_)) => (style_rgb(style.fg), fill.unwrap_or(ground)),
            (Some(style), None) => (style_rgb(style.fg), ground),
            (None, _) => (style_rgb(theme.get(scope::UI_TEXT_MUTED).fg), ground),
        };
        run(*col, name, caption, colors, false);
    }

    scene.nudge([0, 0]);
}

/// What a modal's key line shows, from its left edge to its right.
#[derive(Default)]
pub(crate) struct Footer<'a> {
    /// Key hints, each a bound key and the word for what it does.
    pub(crate) hints: &'a [(&'a str, &'a str)],
    /// Styled spans set flush right, such as a legend of the row marks.
    pub(crate) legend: &'a [(&'a str, Style)],
    /// Styled spans that end the line, such as the context a modal opened in.
    /// Empty for a line with no note.
    pub(crate) note: &'a [(&'a str, Style)],
}

/// The rect of `modal`'s key line, the inner rect's last row and the bottom
/// border's row under it, inside the side borders.
///
/// A body ends on the row above the line, so it loses the inner rect's last
/// row to the line.
pub(crate) fn footer_rect(modal: Rect) -> Rect {
    let rows = FOOTER_ROWS.min(modal.height);
    Rect::new(
        modal.x + 1,
        modal.bottom() - rows,
        modal.width.saturating_sub(2),
        rows,
    )
}

/// Paint a modal's key line into `rect`, which [`footer_rect`] gives, with its
/// key hints from the left and its legend and note at the right.
///
/// Each hint is a key in `ui.key_label` and the word it does in
/// `ui.text.muted`. The spans of the legend and of the note keep their own
/// styles. The legend is always kept, and the first hint
/// that does not clear it, and every hint after it, is left out. The note
/// ends the line, with the legend before it, only when it clears the hints.
///
/// With `plate`, the line is the plate's: its first row blanks to the plate's
/// ground, and every part draws as a run at the [`TypeRole::Caption`] size,
/// [`PLATE_FOOTER_NUDGE`] px off the row, with no fill and no rule. Otherwise
/// every row of `rect` fills with `ui.modal.footer` and the parts draw in the
/// first row's cells. Under a live scene a hairline in `ui.border.inactive`
/// along the top of that row rules the line off from the body above it.
pub(crate) fn modal_footer(
    buf: &mut Buffer,
    rect: Rect,
    footer: &Footer<'_>,
    plate: Option<PlateColors>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    if rect.is_empty() {
        return;
    }
    match plate {
        Some(plate) => paint_plate_footer(buf, rect, footer, plate.ground, theme, scene),
        None => paint_cell_footer(buf, rect, footer, theme, scene),
    }
}

/// Paint `footer` into `rect` as a cell row on `ui.modal.footer`, ruled off at
/// its top, for [`modal_footer`].
///
/// The line keeps one cell clear at each end, and its hints and its groups
/// stand two cells apart.
fn paint_cell_footer(
    buf: &mut Buffer,
    rect: Rect,
    footer: &Footer<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let style = theme.get(scope::UI_MODAL_FOOTER);
    let row_end = rect.x + rect.width;
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..row_end {
            buf[(x, y)].set_char(' ').set_style(style);
        }
    }

    if let Some(color) = style_rgb(theme.get(scope::UI_BORDER_INACTIVE).fg) {
        Bar {
            x: 0,
            y: 0,
            width: rect.width.saturating_mul(16),
            height: 1,
            color,
            radius: 0,
            corners: 0,
        }
        .render(Rect { height: 1, ..rect }, buf, scene);
    }

    let places = place_footer(footer, rect.width.saturating_sub(2), (2, 2), |text| {
        text.width() as u16
    });
    let (x, y) = (rect.x + 1, rect.y);
    let key_style = theme.get(scope::UI_KEY_LABEL);
    let word_style = theme.get(scope::UI_TEXT_MUTED);
    for (&(key, word), at) in footer.hints.iter().zip(places.hints) {
        write_str_clipped(buf, x + at, y, key, key_style, row_end);
        let word_x = x + at + key.width() as u16 + 1;
        write_str_clipped(buf, word_x, y, word, word_style, row_end);
    }
    if let Some(mut at) = places.legend {
        for &(text, span_style) in footer.legend {
            write_str_clipped(buf, x + at, y, text, span_style, row_end);
            at += text.width() as u16;
        }
    }
    if let Some(mut at) = places.note {
        for &(text, span_style) in footer.note {
            write_str_clipped(buf, x + at, y, text, span_style, row_end);
            at += text.width() as u16;
        }
    }
}

/// Paint `footer` into `rect`'s first row as the plate's key line, runs on the
/// plate's `ground`, for [`modal_footer`].
///
/// The hints stand [`PLATE_HINT_GAP`] apart, and the hints, the legend, and the
/// note stand [`PLATE_FOOTER_GAP`] apart. A part whose color has no RGB value
/// draws nothing.
fn paint_plate_footer(
    buf: &mut Buffer,
    rect: Rect,
    footer: &Footer<'_>,
    ground: [u8; 3],
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    blank_plate_row(buf, rect, ground);

    let caption = theme.type_scale(TypeRole::Caption);
    let width = |text: &str| text_run::advance_sixteenths(text.chars().count(), caption);
    let places = place_footer(
        footer,
        rect.width.saturating_mul(16),
        (PLATE_HINT_GAP, PLATE_FOOTER_GAP),
        width,
    );
    let key = style_rgb(theme.get(scope::UI_KEY_LABEL).fg);
    let muted = style_rgb(theme.get(scope::UI_TEXT_MUTED).fg);

    let origin = Rect::new(rect.x, rect.y, 1, 1);
    scene.nudge(PLATE_FOOTER_NUDGE);
    let mut run = |col: u16, text: &str, color: Option<[u8; 3]>| {
        if let (Some(color), false) = (color, text.is_empty()) {
            plate_run(col, text, caption, color, ground, false).render(origin, buf, scene);
        }
    };
    for (&(key_text, word), at) in footer.hints.iter().zip(places.hints) {
        run(at, key_text, key);
        run(at + width(key_text) + width(" "), word, muted);
    }
    if let Some(mut at) = places.legend {
        for &(text, span_style) in footer.legend {
            run(at, text, style_rgb(span_style.fg));
            at += width(text);
        }
    }
    if let Some(mut at) = places.note {
        for &(text, span_style) in footer.note {
            run(at, text, style_rgb(span_style.fg));
            at += width(text);
        }
    }
    scene.nudge([0, 0]);
}

/// A run of `text` that starts `col` sixteenths into a plate's header or key
/// line row, in `color` over an opaque `bg`, at `scale`.
pub(super) fn plate_run(
    col: u16,
    text: &str,
    scale: u16,
    color: [u8; 3],
    bg: [u8; 3],
    bold: bool,
) -> TextRun<'_> {
    TextRun {
        col: i16::try_from(col).unwrap_or(i16::MAX),
        row: 0,
        scale,
        color,
        bg: Some(bg),
        bold,
        italic: false,
        text,
        follow: 0,
        anchor: None,
    }
}

/// Draw a plate row's block caret `col` sixteenths into the row of `origin`,
/// over `at_caret`, the char it covers, at `scale`.
///
/// The caret is a bar in `text`, one char's advance wide, [`CARET_TOP`] under
/// the row's top and [`CARET_HEIGHT`] high. The char draws over it in `ground`
/// with no background of its own, and a caret over a blank draws the bar alone.
pub(super) fn plate_caret(
    col: u16,
    at_caret: &str,
    scale: u16,
    (text, ground): ([u8; 3], [u8; 3]),
    origin: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let x = i16::try_from(col).unwrap_or(i16::MAX);
    Bar {
        x,
        y: CARET_TOP,
        width: text_run::advance_sixteenths(1, scale),
        height: CARET_HEIGHT,
        color: text,
        radius: 0,
        corners: 0,
    }
    .render(origin, buf, scene);

    if at_caret.trim().is_empty() {
        return;
    }
    TextRun {
        col: x,
        row: 0,
        scale,
        color: ground,
        bg: None,
        bold: false,
        italic: false,
        text: at_caret,
        follow: 0,
        anchor: None,
    }
    .render(origin, buf, scene);
}

/// Where the parts of a key line start, in the units of the measure that
/// placed them, as [`place_footer`] gives them.
#[derive(Debug, PartialEq)]
struct FooterPlaces {
    /// Each kept hint's start, in hint order.
    hints: Vec<u16>,
    /// The legend's start, when the footer has a legend.
    legend: Option<u16>,
    /// The note's start, when the note shows.
    note: Option<u16>,
}

/// Place `footer`'s parts along a line `end` units long, with its hints
/// `gaps.0` apart and its groups `gaps.1` apart, measured by `width`.
///
/// The hints start at 0. The legend ends at the line's end, and the first hint
/// that does not end a group gap before it, and every hint after it, stays out.
/// The note ends at the line's end and moves the legend before it, but only
/// when the leftmost of them still clears the kept hints by a group gap.
fn place_footer(
    footer: &Footer<'_>,
    end: u16,
    (hint_gap, group_gap): (u16, u16),
    width: impl Fn(&str) -> u16,
) -> FooterPlaces {
    let has_legend = !footer.legend.is_empty();
    let legend_width: u16 = footer.legend.iter().map(|&(text, _)| width(text)).sum();
    let legend_x = end.saturating_sub(legend_width);
    let limit = match has_legend {
        true => legend_x.saturating_sub(group_gap),
        false => end,
    };

    let mut hints = Vec::with_capacity(footer.hints.len());
    let mut hints_end = None;
    let mut x = 0;
    for &(key, word) in footer.hints {
        let hint_end = x + width(key) + width(" ") + width(word);
        if hint_end > limit {
            break;
        }
        hints.push(x);
        hints_end = Some(hint_end);
        x = hint_end + hint_gap;
    }

    let note_width: u16 = footer.note.iter().map(|&(text, _)| width(text)).sum();
    let note = (!footer.note.is_empty() && note_width <= end).then(|| end - note_width);
    let leftmost = note.map(|note_x| match has_legend {
        true => note_x
            .saturating_sub(group_gap)
            .saturating_sub(legend_width),
        false => note_x,
    });
    let clears = |left: u16| hints_end.is_none_or(|hints_end| left >= hints_end + group_gap);
    match leftmost.filter(|&left| clears(left)) {
        Some(left) => FooterPlaces {
            hints,
            legend: has_legend.then_some(left),
            note,
        },
        None => FooterPlaces {
            hints,
            legend: has_legend.then_some(legend_x),
            note: None,
        },
    }
}

/// The color of a strong line on an overlay, which is the `ui.surface.overlay`
/// stroke, or `ui.border.inactive` in a theme that sets no overlay stroke.
pub(crate) fn line_strong(theme: &Theme) -> Option<[u8; 3]> {
    theme
        .try_get(scope::UI_SURFACE_OVERLAY)
        .and_then(|style| style_rgb(style.fg))
        .or_else(|| style_rgb(theme.get(scope::UI_BORDER_INACTIVE).fg))
}

/// First entry visible when `selected` is showing, given `rows` on screen.
///
/// The window is anchored to the bottom. A selection past it rides the last
/// visible row, and one still above it leaves the list where it is. Every
/// scrolling modal list follows this, so a picker's hit test and its paint
/// agree on which entry a screen row holds.
///
/// A list shorter than its window starts at zero, which is what makes this
/// safe to call before knowing whether scrolling is needed at all.
pub(crate) fn window_start(selected: usize, rows: usize) -> usize {
    selected.saturating_sub(rows.saturating_sub(1))
}

/// Rows of chrome the box spends before its list. The border takes two, the
/// filter prompt one, and the rule under it one more.
const CHROME_ROWS: u16 = 4;

/// Where a target picker's parts sit inside `area`.
///
/// One shape for every jump-target list, so they open the same size and split
/// their body the same way.
pub(crate) struct PickerLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border, starting at the filter prompt.
    pub(crate) inner: Rect,
    /// The candidate rows.
    pub(crate) list: Rect,
    /// The selected candidate's file, when the box is wide enough to show one.
    pub(crate) preview: Option<Rect>,
    /// The key line under the list and the preview, as [`footer_rect`] gives it.
    pub(crate) footer: Rect,
}

/// Lay a target picker out within `area`, or [`None`] when `area` is too small
/// to host it.
///
/// Painting and hit-testing both go through this, so a clicked row names the
/// row drawn there.
pub(crate) fn target_picker_layout(area: Rect, zoom: i8, list_share: u16) -> Option<PickerLayout> {
    let modal = crate::render::chrome::modal_box(
        area,
        (120, 32u16.saturating_add(CHROME_ROWS)),
        (crate::render::chrome::list_modal_width(area), 32),
        (40, 12),
        zoom,
    )?;
    let inner = Block::default().borders(Borders::ALL).inner(modal);
    let footer = footer_rect(modal);

    let body_top = inner.y + 2;
    let body_height = footer.y.saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    let (list, preview) = split_list_preview(
        inner.x,
        body_top,
        inner.width,
        body_height,
        80,
        MIN_PANE_COLUMNS,
        list_share,
    );
    Some(PickerLayout {
        modal,
        inner,
        list,
        preview,
        footer,
    })
}

/// Fixtures for asserting which screen row a picker paints its selection on.
///
/// The theme and the buffer scan only mean anything together. The theme colors
/// `ui.selection` and nothing else, so a scan for that background finds the
/// selected row and can find no other, and a picker that scrolls its selection
/// off-screen highlights nothing at all.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::theme::Theme;
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use stoatty_protocol::command::{self, Command, Edge};

    /// One panel's fill, edges, and corner radius, as [`row_boxes`] reads it.
    pub(crate) type RowBox = (Option<[u8; 3]>, [Edge; 4], u8);

    /// The selection background [`rounded_row_theme`] sets.
    pub(crate) const ROW_SELECT: [u8; 3] = [4, 5, 6];

    /// A theme whose selection and accent are RGB, so a live scene draws a
    /// selected row as its rounded box. The accent lets a row test show that
    /// the box draws no accent.
    pub(crate) fn rounded_row_theme() -> Theme {
        let src = r##"theme t {
            ui.selection.bg = "#040506";
            ui.border.focused.fg = "#070809";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// Each panel in `bytes` that spans exactly `rect`, in the order the panels
    /// go out.
    pub(crate) fn row_boxes(bytes: &[u8], rect: Rect) -> Vec<RowBox> {
        command::decode_stream(bytes)
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Panel(panel)
                    if (panel.left, panel.top, panel.width, panel.height)
                        == (rect.x, rect.y, rect.width, rect.height) =>
                {
                    Some((panel.fill, panel.edges, panel.corner_radius))
                },
                _ => None,
            })
            .collect()
    }

    /// What [`row_boxes`] reads at a rounded selected row in `select`, one box
    /// with 4 px corners, 1 px inside the row's left side and 3 px inside its
    /// right.
    pub(crate) fn rounded_row_boxes(select: [u8; 3]) -> Vec<RowBox> {
        let edges = [
            Edge::Inset(0),
            Edge::Inset(3),
            Edge::Inset(0),
            Edge::Inset(1),
        ];
        vec![(Some(select), edges, 4)]
    }

    /// The background [`selection_theme`] gives the selection.
    pub(crate) const SELECTION_BG: Color = Color::Rgb(255, 0, 0);

    pub(crate) fn selection_theme() -> Theme {
        let src = r##"theme t { ui.selection.bg = "#ff0000"; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The foreground [`match_theme`] gives a fuzzy-match character.
    pub(crate) const MATCH_FG: Color = Color::Rgb(255, 0, 0);

    pub(crate) fn match_theme() -> Theme {
        let src = r##"theme t { ui.search.match.fg = "#ff0000"; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    pub(crate) fn row_text(buf: &Buffer, row: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }
}

/// Cells before the label of a list row.
const LABEL_PAD: u16 = 1;

/// Cells that every list row keeps at its right, a blank and the cell of the
/// modified mark, whether the row holds a mark or not.
///
/// So the height of a result depends on the list width and its label, and on
/// nothing else, which is what lets a pool measure rows it never paints.
const MARK_COLUMNS: u16 = 2;

/// Cells from the start of a label to the chars of its later lines, which
/// hold two blanks, the Wrap Mark, and one blank.
const WRAP_INDENT: u16 = 4;

/// The cells a list `list_width` cells wide gives each label, at least one.
///
/// A label takes all of a row but the [`LABEL_PAD`] before it and the
/// [`MARK_COLUMNS`] after it.
pub(crate) fn label_columns(list_width: u16) -> usize {
    usize::from(list_width.saturating_sub(LABEL_PAD + MARK_COLUMNS)).max(1)
}

/// The results of a path list, and what each label reads from.
///
/// The live list, the pages that glide it, and the hit test all measure and
/// paint a result through this, so the three agree on the rows it takes.
pub(crate) struct PathRows<'a> {
    pub(crate) picklist: &'a PickList,
    pub(crate) git_root: &'a Path,
    pub(crate) home: Option<&'a Path>,
    /// The text before every label, the typed directory of a browse.
    pub(crate) prefix: &'a str,
}

impl PathRows<'_> {
    /// The path of result `result`, or `None` past the last result.
    fn path(&self, result: usize) -> Option<&PathBuf> {
        let &index = self.picklist.filtered.get(result)?;
        self.picklist.base.get(index)
    }

    /// Write the label of result `result` into `out`, and answer whether the
    /// result is a path rather than a relative entry, or `None` past the last
    /// result.
    ///
    /// The label is the prefix, then the path as the list displays it.
    fn label(&self, result: usize, out: &mut String) -> Option<bool> {
        let path = self.path(result)?;
        out.clear();
        out.push_str(self.prefix);
        write_row_display(
            out,
            path,
            self.git_root,
            self.picklist.display_roots.as_deref(),
            self.home,
        );
        Some(path.is_absolute())
    }

    /// The cell rows that result `result` takes where labels are `width`
    /// cells wide, or 0 past the last result.
    ///
    /// `label` and `starts` are scratch that a caller measuring many results
    /// keeps across the calls.
    fn height(
        &self,
        result: usize,
        width: usize,
        label: &mut String,
        starts: &mut Vec<usize>,
    ) -> u16 {
        let Some(is_path) = self.label(result, label) else {
            return 0;
        };
        label_lines(label, is_path, width, starts);
        starts.len() as u16
    }
}

/// The first result that a paint of a path list shows, with the count of that
/// result's cell rows above the paint's area, or of its physical px in a pixel
/// list.
///
/// The live list starts on a whole result. A pooled page starts wherever its
/// first document row falls, and that row is partway through a label when the
/// label wraps across the page's top edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RowStart {
    pub(crate) result: usize,
    pub(crate) skip: u16,
}

/// The paths whose results end their first row in a mark, and the mark they
/// take.
///
/// A row has one mark cell, so a list takes one kind of mark. Each slice is
/// sorted, because [`paint_path_rows`] finds a path in it by binary search.
#[derive(Clone, Copy)]
pub(crate) enum RowMarks<'a> {
    /// Files with changes in the work tree, which end in an `M` in the
    /// `diff.modified` color.
    Modified(&'a [PathBuf]),
    /// Open buffers with unsaved edits, which end in a dot in the
    /// `ui.list.dirty` foreground.
    Dirty(&'a [PathBuf]),
}

/// The results that the window of a path list shows, from `first`, with the
/// rows that each takes.
///
/// A row is a cell row, or a physical px in a pixel list, where a result takes
/// its box and the gap after it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ListWindow {
    pub(crate) first: usize,
    heights: Vec<u32>,
}

impl ListWindow {
    /// The result that window row `row` shows, or `None` past the window's
    /// results.
    pub(crate) fn result_at(&self, row: u32) -> Option<usize> {
        let mut bottom = 0u32;
        for (offset, &height) in self.heights.iter().enumerate() {
            bottom += height;
            if row < bottom {
                return Some(self.first + offset);
            }
        }
        None
    }

    /// The count of results that the window shows whole within `rows` rows.
    pub(crate) fn whole(&self, rows: u32) -> usize {
        let mut bottom = 0u32;
        self.heights
            .iter()
            .take_while(|&&height| {
                bottom += height;
                bottom <= rows
            })
            .count()
    }
}

/// Paint the results of a path list into `area`, each label on as many rows
/// as it needs, with the selected result and fuzzy-match characters
/// highlighted.
///
/// `start` names the first result to show and its rows above `area`. The live
/// list paints the window that [`list_window`] derives, and a pooled page
/// paints from where [`page_start`] puts its first row, so both render
/// identical rows.
///
/// `row_style` paints every result but the selected one. A caller passes its
/// own style to set a whole list apart, such as a list of ignored files.
///
/// The selected result draws through [`chrome::selected_row`] over all of its
/// rows in `area`. With `panels` it is a rounded box in a live scene. A pool
/// page passes `panels` false, because a page captures no panel, and keeps the
/// cells in [`Theme::list_selected`] with an accent bar on each row.
///
/// Every label starts one cell in from the list's left edge, and every row
/// keeps its last [`MARK_COLUMNS`] cells clear of the label. A label wider than
/// its row breaks as [`label_lines`] breaks it, and each later line starts
/// [`WRAP_INDENT`] cells in, under the Wrap Mark in the foreground of
/// [`editor::wrap_mark_style`].
///
/// A path paints its directory in `ui.text.muted` and its name bold in the
/// row's style. The extension that [`chrome::tagged_name`] splits off also
/// takes its tag's color, and a match paints over both, on every line. A path
/// found in `marks` ends its first row in the mark that [`RowMarks`] names. A
/// relative entry, such as a branch name, gets none of these.
///
/// The dot of [`RowMarks::Dirty`] is a round bar under a live scene with an RGB
/// `ui.list.dirty` foreground. The bar straddles the left edge of the row's
/// last cell, so it stands inside the box of a selected row. Otherwise the
/// last cell holds `●` in that scope's style. A theme that leaves the scope
/// unset draws no dot.
///
/// With `pixels` the list is a pixel list, which sets its results at the
/// boards' pixel sizes and writes no cell of `buf`, and `start` counts physical
/// px. A result is a box of its lines with [`pixel_rows::ROW_PAD`] over and
/// under them, and [`pixel_rows::ROW_GAP`] stands between two boxes. Each line
/// draws as text runs on the cell row that holds its top pixel row, under the
/// nudge that sets it there, and only where that top lies in `area`. The same
/// rule places the selected result's box, a rounded bar in the background of
/// [`Theme::list_selected`] with the sides of the cell form's box, and the dot,
/// which stands [`DIRTY_DOT_DROP`] under the top of its box. A page of a
/// resident pool is what draws this, since no row stands on the cell grid.
///
/// A pixel list's marks end [`MARK_INSET`] inside its right edge. The `M`
/// draws at the Caption size on the result's first line, and the dot is
/// [`DIRTY_DOT_SIZE`] across.
///
/// [`chrome::selected_row`]: crate::render::chrome::selected_row
/// [`chrome::tagged_name`]: crate::render::chrome::tagged_name
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_path_rows(
    rows: &PathRows<'_>,
    marks: RowMarks<'_>,
    area: Rect,
    start: RowStart,
    theme: &Theme,
    row_style: Style,
    panels: bool,
    pixels: Option<RowMetrics>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut painter = RowPainter::new(rows, theme, area.width);
    if let Some(metrics) = pixels {
        paint_pixel_rows(
            &mut painter,
            marks,
            area,
            start,
            metrics,
            row_style,
            buf,
            scene,
        );
        return;
    }
    let mark_style = modified_mark_style(theme);
    let area_end = area.x + area.width;

    let mut top = i32::from(area.y) - i32::from(start.skip);
    let mut result = start.result;
    while top < i32::from(area.bottom()) {
        let Some(measured) = painter.measure(result) else {
            break;
        };
        let path = measured.path;
        let bottom = top + measured.lines as i32;
        let shown = top.max(i32::from(area.y))..bottom.min(i32::from(area.bottom()));
        if shown.is_empty() {
            top = bottom;
            result += 1;
            continue;
        }

        let cells = Rect::new(
            area.x,
            shown.start as u16,
            area.width,
            (shown.end - shown.start) as u16,
        );
        let style = match result == rows.picklist.selected {
            true => {
                crate::render::chrome::selected_row(cells, row_style, panels, theme, buf, scene)
            },
            false => {
                for y in cells.top()..cells.bottom() {
                    for x in cells.left()..cells.right() {
                        buf[(x, y)].set_char(' ').set_style(row_style);
                    }
                }
                row_style
            },
        };
        painter.paint_result(measured, area, top, style, buf);

        if measured.is_path && area.width > LABEL_PAD + MARK_COLUMNS && shown.contains(&top) {
            let (x, y) = (area_end - 1, top as u16);
            match marks {
                RowMarks::Modified(paths) if paths.binary_search(path).is_ok() => {
                    buf[(x, y)].set_char('M').set_style(mark_style);
                },
                RowMarks::Dirty(paths) if paths.binary_search(path).is_ok() => {
                    dirty_mark(x, y, theme, buf, scene);
                },
                RowMarks::Modified(_) | RowMarks::Dirty(_) => {},
            }
        }

        top = bottom;
        result += 1;
    }
}

/// The logical px from the top of a pixel list's result box to the top of its
/// dot, as FinderShared sets the dot's `margin-top`.
const DIRTY_DOT_DROP: u16 = 6;

/// The logical px across the dot of an unsaved buffer in a pixel list.
const DIRTY_DOT_SIZE: u16 = 6;

/// The logical px from a pixel list's right edge to the right end of a row's
/// mark, which is the 3 px that the selected box stands inside the list and
/// the 8 px of the row's padding.
const MARK_INSET: u16 = 11;

/// What every result of one paint of a path list shares, with the scratch that
/// its results reuse.
struct RowPainter<'r, 'a> {
    rows: &'r PathRows<'a>,
    theme: &'r Theme,
    match_style: Style,
    dir_style: Style,
    wrap_fg: Option<Color>,
    /// The chars that a label's first line holds.
    width: usize,
    prefix_len: u32,
    label: String,
    starts: Vec<usize>,
    /// Rows past the eagerly indexed block derive their offsets into this, one
    /// row at a time.
    derived: Vec<u32>,
    matching: fuzzy::Scratch,
}

/// A result whose label [`RowPainter::measure`] broke into lines, which the
/// painter holds until it measures another.
#[derive(Clone, Copy)]
struct Measured<'r> {
    result: usize,
    is_path: bool,
    path: &'r PathBuf,
    lines: u32,
}

/// The mark that ends a result's first line in a pixel list, which stands
/// [`MARK_INSET`] inside the list's right edge.
#[derive(Clone, Copy)]
enum PixelMark {
    /// A glyph drawn at the Caption size in a color, which is the `M` of a
    /// modified file or the `●` of an unsaved buffer whose dot has no RGB
    /// color.
    Glyph(char, [u8; 3]),
    /// The round dot of an unsaved buffer in a color.
    Dot([u8; 3]),
}

impl<'r, 'a> RowPainter<'r, 'a> {
    fn new(rows: &'r PathRows<'a>, theme: &'r Theme, list_width: u16) -> Self {
        Self {
            rows,
            theme,
            match_style: theme.list_match(),
            dir_style: theme.get(scope::UI_TEXT_MUTED),
            wrap_fg: editor::wrap_mark_style(theme).fg,
            width: label_columns(list_width),
            prefix_len: rows.prefix.chars().count() as u32,
            label: String::new(),
            starts: Vec::new(),
            derived: Vec::new(),
            matching: fuzzy::Scratch::default(),
        }
    }

    /// Break the label of result `result` into lines, or `None` past the last
    /// result.
    fn measure(&mut self, result: usize) -> Option<Measured<'r>> {
        let rows = self.rows;
        let is_path = rows.label(result, &mut self.label)?;
        let path = rows.path(result)?;
        label_lines(&self.label, is_path, self.width, &mut self.starts);
        Some(Measured {
            result,
            is_path,
            path,
            lines: self.starts.len() as u32,
        })
    }

    /// Paint the lines of `measured` that fall on the rows of `area`, where its
    /// first line is row `top`, in `style`.
    ///
    /// `measured` is the result this painter measured last. The cells under the
    /// lines keep whatever fill the caller gave them.
    fn paint_result(
        &mut self,
        measured: Measured<'_>,
        area: Rect,
        top: i32,
        style: Style,
        buf: &mut Buffer,
    ) {
        let text_end = (area.x + area.width).saturating_sub(MARK_COLUMNS);
        let wrap_style = match self.wrap_fg {
            Some(fg) => style.fg(fg),
            None => style,
        };

        let label = self.label.as_str();
        let name_col = if measured.is_path {
            name_start(label)
        } else {
            0
        };
        let label_len = label.chars().count();
        let ext = match measured.is_path {
            true => crate::render::chrome::tagged_name(measured.path),
            false => None,
        }
        .map(|tagged| {
            let ext_col = label_len.saturating_sub(tagged.suffix.chars().count());
            (ext_col, self.theme.get(tagged.tag.scope))
        });
        let indices =
            self.rows
                .picklist
                .row_indices(measured.result, &mut self.derived, &mut self.matching);

        let rows = i32::from(area.y)..i32::from(area.bottom());
        for (line, &line_start) in self.starts.iter().enumerate() {
            let y = top + line as i32;
            if !rows.contains(&y) {
                continue;
            }
            let y = y as u16;
            let line_end = self.starts.get(line + 1).copied().unwrap_or(label_len);
            let text_x = match line {
                0 => area.x + LABEL_PAD,
                _ => {
                    let mark_x = area.x + LABEL_PAD + 2;
                    if mark_x < text_end {
                        buf[(mark_x, y)].set_char('\u{21b3}').set_style(wrap_style);
                    }
                    area.x + LABEL_PAD + WRAP_INDENT
                },
            };
            let text = &label[char_byte(label, line_start)..char_byte(label, line_end)];
            write_str_clipped(buf, text_x, y, text, style, text_end);

            for (offset, label_col) in (line_start..line_end).enumerate() {
                let col = text_x + offset as u16;
                if col >= text_end {
                    break;
                }
                let cell = &mut buf[(col, y)];
                if label_col < name_col {
                    cell.set_style(self.dir_style);
                } else if measured.is_path {
                    cell.set_style(Style::default().add_modifier(Modifier::BOLD));
                }
                if let Some((ext_col, ext_style)) = ext
                    && label_col >= ext_col
                {
                    cell.set_style(ext_style);
                }
                let label_col = label_col as u32;
                // The literal `prefix` is never matched. The picklist's indices
                // are offsets into the part after it, so shift them past the
                // prefix.
                if label_col >= self.prefix_len
                    && indices
                        .binary_search(&(label_col - self.prefix_len))
                        .is_ok()
                {
                    cell.set_style(self.match_style);
                }
            }
        }
    }
}

/// Paint the results of a path list into `area` from `start` as the pixel
/// list that [`paint_path_rows`] describes.
#[allow(clippy::too_many_arguments)]
fn paint_pixel_rows(
    painter: &mut RowPainter<'_, '_>,
    marks: RowMarks<'_>,
    area: Rect,
    start: RowStart,
    metrics: RowMetrics,
    row_style: Style,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let theme = painter.theme;
    let selected = theme.list_selected(scope::UI_SELECTION);
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let line_height = i64::from(metrics.cell[1]);
    let height = i64::from(area.height) * line_height;
    let inside = |y: i64| (0..height).contains(&y);
    let mut placer = Placer::default();
    let mut scratch = Buffer::empty(Rect::default());

    let mut y = -i64::from(start.skip);
    let mut result = start.result;
    while y < height {
        let Some(measured) = painter.measure(result) else {
            break;
        };
        let lines = measured.lines;
        let style = match result == painter.rows.picklist.selected {
            true => {
                if let Some(color) = style_rgb(selected.bg).filter(|_| inside(y)) {
                    let left = metrics.cols(1);
                    let bar = PixelBar {
                        x: left as i16,
                        y: y as u32,
                        width: (area.width.saturating_mul(16))
                            .saturating_sub(left + metrics.cols(3)),
                        height: metrics.box16(lines as u16),
                        color,
                        radius: SELECTED_ROW_RADIUS,
                    };
                    pixel_rows::bar(&mut placer, &metrics, area, &bar, buf, scene);
                }
                selected
            },
            false => row_style,
        };

        let cells = Rect::new(0, 0, area.width, lines as u16);
        scratch.resize(cells);
        scratch.reset();
        painter.paint_result(measured, cells, 0, style, &mut scratch);
        let mark = match area.width > LABEL_PAD + MARK_COLUMNS {
            true => pixel_mark(marks, measured, theme, fallback),
            false => None,
        };
        let mark_end = area
            .width
            .saturating_mul(16)
            .saturating_sub(metrics.cols(MARK_INSET));

        for line in 0..lines {
            let top = y + i64::from(metrics.px(ROW_PAD)) + i64::from(line) * line_height;
            if !inside(top) {
                continue;
            }
            let at = RunsAt {
                col: 0,
                scale: FULL_CELL_SCALE,
                anchor: placer.line(&metrics, area, top as u32, scene),
                fallback,
            };
            pixel_rows::emit_cells(&scratch, line as u16, 0..area.width, &at, buf, scene);
            if let (0, Some(PixelMark::Glyph(glyph, color))) = (line, mark) {
                let scale = theme.type_scale(TypeRole::Caption);
                let mut utf8 = [0; 4];
                TextRun {
                    col: mark_end.saturating_sub(text_run::advance_sixteenths(1, scale)) as i16,
                    row: 0,
                    scale,
                    color,
                    bg: None,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                    text: glyph.encode_utf8(&mut utf8),
                }
                .render(at.anchor, buf, scene);
            }
        }

        let dot_top = y + i64::from(metrics.px(DIRTY_DOT_DROP));
        if let Some(PixelMark::Dot(color)) = mark
            && inside(dot_top)
        {
            let size = metrics.cols(DIRTY_DOT_SIZE);
            let bar = PixelBar {
                x: mark_end.saturating_sub(size) as i16,
                y: dot_top as u32,
                width: size,
                height: metrics.rows16(DIRTY_DOT_SIZE),
                color,
                radius: (DIRTY_DOT_SIZE / 2) as u8,
            };
            pixel_rows::bar(&mut placer, &metrics, area, &bar, buf, scene);
        }

        y += i64::from(metrics.pitch(lines));
        result += 1;
    }
    placer.end(scene);
}

/// The mark that `marks` gives `measured` in a pixel list, or `None` for a
/// result that takes none.
///
/// A mark with no RGB value of its own takes `fallback`, the color a run of
/// the list falls back to, and a dot with none draws as the `●` glyph.
fn pixel_mark(
    marks: RowMarks<'_>,
    measured: Measured<'_>,
    theme: &Theme,
    fallback: [u8; 3],
) -> Option<PixelMark> {
    if !measured.is_path {
        return None;
    }
    match marks {
        RowMarks::Modified(paths) if paths.binary_search(measured.path).is_ok() => {
            let color = style_rgb(modified_mark_style(theme).fg).unwrap_or(fallback);
            Some(PixelMark::Glyph('M', color))
        },
        RowMarks::Dirty(paths) if paths.binary_search(measured.path).is_ok() => {
            let style = theme.try_get(scope::UI_LIST_DIRTY)?;
            Some(match style_rgb(style.fg) {
                Some(color) => PixelMark::Dot(color),
                None => PixelMark::Glyph('\u{25cf}', fallback),
            })
        },
        RowMarks::Modified(_) | RowMarks::Dirty(_) => None,
    }
}

/// The style of the `M` that ends a modified file's row.
///
/// `ui.list.modified` names it, and a theme that leaves that scope unset
/// keeps `diff.modified`.
fn modified_mark_style(theme: &Theme) -> Style {
    theme
        .try_get(scope::UI_LIST_MODIFIED)
        .unwrap_or_else(|| theme.get(scope::DIFF_MODIFIED))
}

/// Draw the dot of an unsaved buffer's result in the cell at column `x` and
/// row `y`, as [`paint_path_rows`] describes it.
fn dirty_mark(
    x: u16,
    y: u16,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(style) = theme.try_get(scope::UI_LIST_DIRTY) else {
        return;
    };
    match style_rgb(style.fg).filter(|_| scene.live()) {
        Some(color) => dirty_dot(color).render(Rect::new(x, y, 1, 1), buf, scene),
        None => {
            buf[(x, y)].set_char('\u{25cf}').set_style(style);
        },
    }
}

/// The bar of an unsaved buffer's dot in `color`, placed on the cell that ends
/// the result's first row.
fn dirty_dot(color: [u8; 3]) -> Bar {
    // Sixteenths run along each axis of the 9 by 18 px cell, so 11 across and 6
    // down are close to 6 px each way, and a 3 px radius rounds the bar to a
    // dot. The bar starts 5 sixteenths left of the cell.
    Bar {
        x: -5,
        y: 5,
        width: 11,
        height: 6,
        color,
        radius: 3,
        corners: 0,
    }
}

/// Break `label` into the lines that a list row shows it on, where the first
/// line holds `width` chars, and write the char index each line starts at into
/// `starts`.
///
/// Each later line holds [`WRAP_INDENT`] chars fewer, at least one. A path
/// breaks only after a `/`. A line takes the segments that fit whole, where a
/// segment ends after its `/`. A segment longer than a whole line starts on a
/// fresh line and breaks at the last cell of each line, so no char is cut. A
/// relative entry is one segment, so it breaks at the last cell of each line.
fn label_lines(label: &str, is_path: bool, width: usize, starts: &mut Vec<usize>) {
    starts.clear();
    starts.push(0);
    let total = label.chars().count();
    if total <= width {
        return;
    }

    let later = width.saturating_sub(usize::from(WRAP_INDENT)).max(1);
    let (mut budget, mut used) = (width.max(1), 0);
    let mut segment_start = 0;
    for (index, ch) in label.chars().enumerate() {
        let end = index + 1;
        if end < total && !(is_path && ch == '/') {
            continue;
        }

        let len = end - segment_start;
        if used + len <= budget {
            used += len;
        } else {
            let (mut at, mut rest) = (segment_start, len);
            if used > 0 {
                starts.push(at);
                budget = later;
            }
            while rest > budget {
                at += budget;
                rest -= budget;
                starts.push(at);
                budget = later;
            }
            used = rest;
        }
        segment_start = end;
    }
}

/// The byte offset of char `index` of `text`, or the length of `text` past its
/// last char.
fn char_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The window of a path list `list_rows` rows tall whose labels take
/// [`label_columns`] of `list_width`.
///
/// The live list paints this window, the hit test reads it, and the pool
/// scrolls to its first result, so the three agree on what each row shows. It
/// measures only the results it shows.
///
/// With `pixels` the window is a pixel list's, whose rows are physical px. A
/// result takes the px of its box and the gap after it, and the list's rows
/// give their px and one gap more, since the gap after the last result stands
/// past the list.
///
/// See also:
/// - [`window_from`] for the rule that picks the results.
pub(crate) fn list_window(
    rows: &PathRows<'_>,
    list_width: u16,
    list_rows: u16,
    pixels: Option<RowMetrics>,
) -> ListWindow {
    let width = label_columns(list_width);
    let (mut label, mut starts) = (String::new(), Vec::new());
    let mut lines = |result| u32::from(rows.height(result, width, &mut label, &mut starts));
    let (len, selected) = (rows.picklist.filtered.len(), rows.picklist.selected);
    match pixels {
        Some(metrics) => window_from(len, selected, metrics.extent(list_rows), |result| {
            metrics.pitch(lines(result))
        }),
        None => window_from(len, selected, u32::from(list_rows), lines),
    }
}

/// The window of `len` results in `rows` rows around result `selected`, where
/// result `i` takes `height(i)` rows.
///
/// The window ends with the selected result and holds that result whole. It
/// starts at the first result when the results from the first fit, and it
/// keeps no state, as [`window_start`] keeps none. From the selected result it
/// takes each result above while the rows stay at most `rows`, then the results
/// after the selected one until the rows reach `rows`. A selection past the
/// last result counts as the last result.
fn window_from(
    len: usize,
    selected: usize,
    rows: u32,
    mut height: impl FnMut(usize) -> u32,
) -> ListWindow {
    let Some(last) = len.checked_sub(1) else {
        return ListWindow {
            first: 0,
            heights: Vec::new(),
        };
    };
    let selected = selected.min(last);

    let selected_height = height(selected);
    let mut used = selected_height;
    let mut heights = Vec::new();
    let mut first = selected;
    while first > 0 {
        let above = height(first - 1);
        if used + above > rows {
            break;
        }
        used += above;
        heights.push(above);
        first -= 1;
    }
    heights.reverse();
    heights.push(selected_height);

    let mut next = selected + 1;
    while next <= last && used < rows {
        let below = height(next);
        used += below;
        heights.push(below);
        next += 1;
    }
    ListWindow { first, heights }
}

/// Measure the document rows of `picklist`'s results through result `through`
/// into its [`RowTops`], for a list `list_width` cells wide whose labels start
/// with `prefix`.
///
/// The table measures each result once. A refilter, a new list width, or a new
/// prefix starts it over, since each changes the rows a result takes. A pool
/// calls this before it pages the list, with `through` far enough past the
/// selection to cover the pages it reads.
pub(crate) fn ensure_row_tops(
    picklist: &mut PickList,
    git_root: &Path,
    home: Option<&Path>,
    prefix: &str,
    list_width: u16,
    through: usize,
) {
    let len = picklist.filtered.len();
    let generation = picklist.filter_generation;
    let mut table = picklist
        .row_tops
        .take()
        .filter(|table| table.holds(generation, len, list_width, prefix))
        .unwrap_or_else(|| RowTops {
            generation,
            len,
            width: list_width,
            prefix: prefix.to_owned(),
            tops: vec![0],
        });

    let rows = PathRows {
        picklist,
        git_root,
        home,
        prefix,
    };
    let width = label_columns(list_width);
    let (mut label, mut starts) = (String::new(), Vec::new());
    let end = through.saturating_add(1).min(len);
    while let Some(&top) = table.tops.last()
        && table.tops.len() <= end
    {
        let result = table.tops.len() - 1;
        let height = rows.height(result, width, &mut label, &mut starts);
        table.tops.push(top + u32::from(height));
    }
    picklist.row_tops = Some(table);
}

/// The document row of each result of `picklist` measured so far, for a list
/// `list_width` cells wide whose labels start with `prefix`, or `None` when no
/// table holds for those.
///
/// Entry `i` is the row that result `i` starts on, and the last entry is the
/// row after the last measured result.
pub(crate) fn row_tops<'a>(
    picklist: &'a PickList,
    list_width: u16,
    prefix: &str,
) -> Option<&'a [u32]> {
    picklist
        .row_tops
        .as_ref()
        .filter(|table| {
            table.holds(
                picklist.filter_generation,
                picklist.filtered.len(),
                list_width,
                prefix,
            )
        })
        .map(|table| table.tops.as_slice())
}

/// Where a pooled page whose first document row is `row` starts, from the
/// document rows `tops` of [`row_tops`], or `None` at or past the row after the
/// last measured result.
///
/// The page starts on the last result whose first row is at most `row`, with
/// the rows of that result above `row` skipped.
pub(crate) fn page_start(tops: &[u32], row: u32) -> Option<RowStart> {
    let &end = tops.last()?;
    if row >= end {
        return None;
    }
    let result = tops.partition_point(|&top| top <= row) - 1;
    Some(RowStart {
        result,
        skip: (row - tops[result]) as u16,
    })
}

/// The char index where `label`'s last path component starts, which is the
/// name a row or the preview header paints apart from its directory.
pub(crate) fn name_start(label: &str) -> usize {
    match label.rfind('/') {
        Some(slash) => label[..=slash].chars().count(),
        None => 0,
    }
}

/// Paint the picker's preview pane by rendering its scratch editor into `area`,
/// and return the rows the editor is laid out in. A no-op for an empty rect.
///
/// A file's or a buffer's text leads each line with its number, in a column
/// of [`preview_number_columns`] cells at the pane's left. The editor's
/// `gutter_width` records the column, so the pool that glides the pane pages
/// the same numbers. Such a pane marks its [`Preview::target_line`] as
/// [`paint_target_row`] draws it.
///
/// A live scene with a scaled cell lays the editor out at Preview Scale, in
/// the grid [`preview_grid`] gives, and records it in [`Preview::scaled`]. The
/// paint goes into [`Preview::scratch`] for the layout alone, and `area` blanks
/// to the editor surface, since the preview's resident pool draws the text.
pub(crate) fn render_picker_preview(
    preview: &mut Preview,
    area: Rect,
    theme: &Theme,
    chrome: &ResolvedChrome,
    ws: &mut Workspace,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    preview.scaled = None;
    if area.width == 0 || area.height == 0 {
        return area.height;
    }
    let numbers = preview.numbers_lines();
    let target_line = preview.target_line;
    let Some(editor) = ws.editors.get_mut(preview.editor) else {
        return area.height;
    };

    let grid = preview_grid(area, scene.scaled_cell()).filter(|_| scene.live());
    let column = NumberColumn::new(numbers, grid.is_some());
    let Some((rows, cols)) = grid else {
        paint_preview_editor(editor, column, target_line, area, theme, chrome, buf, scene);
        return area.height;
    };

    let laid_out = Rect::new(0, 0, cols, rows);
    if preview.scratch.area != laid_out {
        preview.scratch.resize(laid_out);
    }
    let mut dead = stoat_widgets::ApcScene::new();
    dead.set_live(false);
    paint_preview_editor(
        editor,
        column,
        target_line,
        laid_out,
        theme,
        chrome,
        &mut preview.scratch,
        &mut dead,
    );
    crate::render::clear_surface(area, buf, theme);
    preview.scaled = Some(ScaledPreview { area, rows, cols });
    rows
}

/// The 256ths of the grid cell that a cell of a preview at Preview Scale takes.
///
/// 154 is the step nearest the boards' 0.6, and gives a 5 by 11 pixel cell at
/// the 9 by 18 pixel grid cell, the boards' 11 px row.
pub(crate) const PREVIEW_SCALE: u16 = 154;

/// The `(rows, cols)` of Preview Scale cells that a preview over `area` lays
/// its source out in, or `None` without a scaled cell, where it keeps the grid.
pub(crate) fn preview_grid(area: Rect, cell: Option<[u16; 2]>) -> Option<(u16, u16)> {
    cell.map(|cell| pool::scaled_grid(area.width, area.height, cell, PREVIEW_SCALE))
}

/// Paint `editor` into `area` of `buf` as a preview, and record in its
/// `gutter_width` the number column `column` leads its lines with.
///
/// A preview with a number column marks the row of 0-based `target_line` as
/// [`paint_target_row`] draws it.
#[allow(clippy::too_many_arguments)]
fn paint_preview_editor(
    editor: &mut EditorState,
    column: NumberColumn,
    target_line: Option<u32>,
    area: Rect,
    theme: &Theme,
    chrome: &ResolvedChrome,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let fallback = theme.get(scope::UI_TEXT);

    // The paint writes `gutter_width` on most paths, but a scroll past the end
    // returns before the write, so the column is set here on both branches.
    let scaled = match column {
        NumberColumn::None => {
            render_editor(editor, area, fallback, theme, Some(chrome), buf, false);
            editor.gutter_width = 0;
            return;
        },
        NumberColumn::Grid => false,
        NumberColumn::Scaled => true,
    };

    let line_count = editor.display_map.buffer_snapshot().line_count();
    let columns = preview_number_columns(line_count, scaled);
    let text = Rect {
        x: area.x + columns,
        width: area.width.saturating_sub(columns),
        ..area
    };
    render_editor(editor, text, fallback, theme, Some(chrome), buf, false);
    let snapshot = editor.display_map.snapshot();
    let top_row = editor.scroll_row;
    paint_preview_numbers(
        &snapshot,
        top_row,
        area,
        theme,
        buf,
        scene,
        scaled,
        target_line,
    );
    paint_target_row(
        &snapshot,
        top_row,
        area,
        target_line,
        scaled,
        theme,
        buf,
        scene,
    );
    editor.gutter_width = columns;
}

/// The column a picker preview numbers its lines in, which a page that glides
/// the preview paints in place of a pane's line-number gutter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NumberColumn {
    /// No column. A preview of a directory listing, a terminal screen, or a
    /// picker's own text numbers nothing, and a pane's page keeps its
    /// line-number gutter.
    None,
    /// The column at the grid's cell.
    Grid,
    /// The column at Preview Scale, ruled off from the text.
    Scaled,
}

impl NumberColumn {
    /// The column of a preview that numbers its lines when `numbers` is set,
    /// at Preview Scale when `scaled` is.
    pub(crate) fn new(numbers: bool, scaled: bool) -> NumberColumn {
        match (numbers, scaled) {
            (false, _) => NumberColumn::None,
            (true, false) => NumberColumn::Grid,
            (true, true) => NumberColumn::Scaled,
        }
    }
}

/// Sixteenths between a preview number's right edge and its line's text, the
/// board's 14 px at the 9 px cell.
const PREVIEW_NUMBER_GAP: u16 = 24;

/// The fewest digits a preview's number column holds.
///
/// Four digits and the cell before the text are the board's 44 px column, so
/// a short file's text starts where a long file's does.
const PREVIEW_NUMBER_DIGITS: u16 = 4;

/// Cells a scaled preview's number column takes past its digits, which hold
/// its rule and the gap from the rule to the text.
const PREVIEW_RULE_CELLS: u16 = 2;

/// Sixteenths of a cell from a scaled preview's digits to its rule, the
/// board's 4 px at the 5 px Preview Scale cell.
const PREVIEW_RULE_INSET: u16 = 13;

/// Sixteenths of a cell that a scaled preview's rule is wide, the board's 1 px
/// at the 5 px Preview Scale cell.
const PREVIEW_RULE_WIDTH: u16 = 3;

/// Sixteenths of a cell that the accent bar on a scaled preview's target row
/// is wide, the board's 2 px at the 5 px Preview Scale cell.
const TARGET_BAR_SCALED: u16 = 6;

/// Sixteenths of a cell that the accent bar on a grid preview's target row is
/// wide, the board's 2 px at the 9 px grid cell.
const TARGET_BAR_GRID: u16 = 4;

/// Cells the number column of a preview over `line_count` lines takes.
///
/// The column holds the widest number, at least [`PREVIEW_NUMBER_DIGITS`]
/// wide, and one cell before the text. A `scaled` column, at Preview Scale,
/// takes [`PREVIEW_RULE_CELLS`] there for its rule, the board's 4 px, 1 px,
/// and 4 px.
pub(crate) fn preview_number_columns(line_count: u32, scaled: bool) -> u16 {
    let before_text = match scaled {
        true => PREVIEW_RULE_CELLS,
        false => 1,
    };
    editor::gutter_digits(line_count).max(PREVIEW_NUMBER_DIGITS) + before_text
}

/// Paint the numbers of a preview's lines from display row `top_row` into the
/// number column at `area`'s left, and return the column's width in cells.
///
/// Each line takes its absolute number on its first row. A wrapped line's
/// later rows and the rows past the end of the text take none. The empty line
/// after a trailing newline takes one, since the cursor reaches it.
///
/// A live scene with an RGB `ui.text.muted` draws each number as a text run at
/// the caption scale, with its right edge [`PREVIEW_NUMBER_GAP`] sixteenths
/// before the text. Any other paint writes the digits in that style into the
/// column's cells, right-aligned before one blank cell.
///
/// A `scaled` column, at Preview Scale, ends its numbers where its digits end
/// and rules them off from the text, as [`paint_preview_rule`] draws it.
///
/// The number on the row that [`paint_target_row`] marks for 0-based
/// `target_line` takes `ui.text` in place of `ui.text.muted`.
///
/// The live preview and the pages that glide it both paint their numbers
/// through this, so the numbers hold still across the handoff between the two.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_preview_numbers(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
    scaled: bool,
    target_line: Option<u32>,
) -> u16 {
    let width = preview_number_columns(snapshot.buffer_line_count(), scaled);
    let visible = snapshot
        .line_count()
        .saturating_sub(top_row)
        .min(u32::from(area.height));
    let (lines, _) = editor::gutter_geometry(snapshot, top_row, visible);

    let muted = theme.get(scope::UI_TEXT_MUTED);
    let color = style_rgb(muted.fg).filter(|_| scene.live());
    let target = theme.get(scope::UI_TEXT);
    let target_color = style_rgb(target.fg);
    let target_y = target_line.and_then(|line| target_row(snapshot, top_row, area, line));
    let scale = theme.type_scale(TypeRole::Caption);
    let (run_end, digits_end) = match scaled {
        true => (
            (width - PREVIEW_RULE_CELLS) * 16,
            area.x + width - PREVIEW_RULE_CELLS,
        ),
        false => (width * 16 - PREVIEW_NUMBER_GAP, area.x + width - 1),
    };

    let mut runs = TextRunBatch::default();
    let mut number = String::new();
    let mut y = area.y;
    for (line, height) in lines {
        number.clear();
        write!(number, "{line}").expect("writing to a String is infallible");
        let on_target = target_y == Some(y);

        match color {
            Some(color) => {
                let advance = text_run::advance_sixteenths(number.len(), scale);
                runs.push(
                    Rect::new(area.x, y, 1, 1),
                    TextRun {
                        col: run_end.saturating_sub(advance) as i16,
                        row: 0,
                        scale,
                        color: target_color.filter(|_| on_target).unwrap_or(color),
                        bg: None,
                        text: &number,
                        bold: false,
                        italic: false,
                        follow: 0,
                        anchor: None,
                    },
                );
            },
            None => {
                let x = digits_end.saturating_sub(number.len() as u16);
                let end_x = digits_end.min(area.x + area.width);
                let style = match on_target {
                    true => target,
                    false => muted,
                };
                write_str_clipped(buf, x, y, &number, style, end_x);
            },
        }
        y += height;
    }
    runs.flush(scene);
    if scaled {
        paint_preview_rule(width, area, theme, buf, scene);
    }

    width
}

/// Rule off a scaled preview's number column, `columns` cells wide, down all
/// of `area`'s rows, as the PreviewZoom board does.
///
/// The rule is a bar [`PREVIEW_RULE_WIDTH`] sixteenths wide and
/// [`PREVIEW_RULE_INSET`] sixteenths past the digits. It takes `ui.divider`,
/// or `ui.border.inactive` where that sets no color, as the finder header's
/// rule does. A dead scene draws no rule, and neither does a theme with
/// neither color.
fn paint_preview_rule(
    columns: u16,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rule = theme
        .try_get(scope::UI_DIVIDER)
        .filter(|style| style.fg.is_some())
        .unwrap_or_else(|| theme.get(scope::UI_BORDER_INACTIVE));
    let Some(color) = style_rgb(rule.fg).filter(|_| scene.live()) else {
        return;
    };

    Bar {
        x: ((columns - PREVIEW_RULE_CELLS) * 16 + PREVIEW_RULE_INSET) as i16,
        y: 0,
        width: PREVIEW_RULE_WIDTH,
        height: area.height.saturating_mul(16),
        color,
        radius: 0,
        corners: 0,
    }
    .render(area, buf, scene);
}

/// Mark the row that 0-based `target_line` starts on, in a preview painted from
/// display row `top_row` into `area`, as the row a picker's result lands on.
///
/// The row's cells across `area` take the background of
/// [`Theme::list_selected`]. A live scene also draws an accent bar in
/// `ui.border.focused` at the row's left edge. The bar is the PreviewZoom
/// board's 2 px, [`TARGET_BAR_SCALED`] sixteenths of a `scaled` cell or
/// [`TARGET_BAR_GRID`] of a grid cell. A `None` line, or one that starts on no
/// row of `area`, draws nothing.
///
/// The live preview and the pages that glide it both mark the row through
/// this, after their text, so the mark holds still across the handoff.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_target_row(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    area: Rect,
    target_line: Option<u32>,
    scaled: bool,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(y) = target_line.and_then(|line| target_row(snapshot, top_row, area, line)) else {
        return;
    };

    if let Some(bg) = theme.list_selected(scope::UI_SELECTION).bg {
        for x in area.left()..area.right() {
            buf[(x, y)].set_bg(bg);
        }
    }

    let accent = theme.get(scope::UI_BORDER_FOCUSED);
    let Some(color) = style_rgb(accent.fg).filter(|_| scene.live()) else {
        return;
    };
    let width = match scaled {
        true => TARGET_BAR_SCALED,
        false => TARGET_BAR_GRID,
    };
    Bar {
        x: 0,
        y: 0,
        width,
        height: 16,
        color,
        radius: 0,
        corners: 0,
    }
    .render(Rect::new(area.x, y, 1, 1), buf, scene);
}

/// The row of `area` that 0-based buffer `line` starts on, in a preview painted
/// from display row `top_row`, or `None` when that row is not in `area`.
///
/// A line past the buffer's end starts on no row, and neither does a wrapped
/// line that the pane opens partway down.
fn target_row(snapshot: &DisplaySnapshot, top_row: u32, area: Rect, line: u32) -> Option<u16> {
    if line >= snapshot.buffer_line_count() {
        return None;
    }

    let offset = snapshot
        .buffer_to_display(Point::new(line, 0))
        .row
        .checked_sub(top_row)?;
    (offset < u32::from(area.height)).then(|| area.y + offset as u16)
}

/// The units of a list pane's share of its body, which ten thousand of make
/// the whole body.
///
/// A dragged separator stores the share its pointer lands on, and the layout
/// reads it back as columns. A unit finer than one column of the widest body
/// lets the layout recover the exact column the pointer left. A whole percent
/// is coarser than a column on a body wider than 100 columns, so it moves the
/// separator a column off the pointer there.
pub(crate) const SHARE_SCALE: u32 = 10_000;

/// Share of a modal's body its list pane takes, in [`SHARE_SCALE`] units,
/// before the user drags the separator to a share of their own.
///
/// One value for the whole modal family, so an untouched finder, symbol list,
/// code search, and commit picker all divide their body the same way.
pub(crate) const DEFAULT_LIST_SHARE: u16 = 4_000;

/// The cells that `share`, in [`SHARE_SCALE`] units, takes of `extent` cells,
/// rounded down.
///
/// See also:
/// - [`SplitSeparator::share_at`](crate::app::SplitSeparator::share_at), which rounds a dragged
///   share up against this rounding down.
pub(crate) fn share_of(extent: u16, share: u16) -> u16 {
    let cells = u32::from(extent) * u32::from(share) / SHARE_SCALE;
    u16::try_from(cells).unwrap_or(u16::MAX)
}

/// Columns each pane of a side-by-side modal keeps.
///
/// A list narrower than this cannot show a path worth reading, and a preview
/// narrower cannot show code, so the split refuses to squeeze either below it
/// however the share is set or dragged.
pub(crate) const MIN_PANE_COLUMNS: u16 = 24;

/// Split a picker body rect into a result list and an optional preview pane.
///
/// The preview appears only when `width >= wide_threshold`, where the list
/// takes `list_share` of the width, in [`SHARE_SCALE`] units, floored at
/// `min_list`, and the preview the rest past a one-cell separator. Below the
/// threshold the list takes the full width and there is no preview.
pub(crate) fn split_list_preview(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    wide_threshold: u16,
    min_list: u16,
    list_share: u16,
) -> (Rect, Option<Rect>) {
    if width >= wide_threshold {
        let list_width = share_of(width, list_share).max(min_list);
        let preview_width = width.saturating_sub(list_width + 1);
        (
            Rect::new(x, y, list_width, height),
            Some(Rect::new(x + list_width + 1, y, preview_width, height)),
        )
    } else {
        (Rect::new(x, y, width, height), None)
    }
}

/// Label columns a list fitted to its labels keeps, however short its labels
/// are.
const LIST_MIN_COLUMNS: u16 = 30;

/// Columns of text a fitted preview keeps, so a line of code reads whole beside
/// a list of short labels.
const PREVIEW_TEXT_COLUMNS: u16 = 100;

/// Body columns under which a fitted finder shows its list alone, with no
/// preview beside it.
const FITTED_SPLIT_MIN_COLUMNS: u16 = 80;

/// The columns of a list fitted to its labels, where `widest` is the most chars
/// of a label on show.
///
/// The labels keep [`LIST_MIN_COLUMNS`] however short they are. The list adds
/// the pad cell before each label and the two mark cells after it.
pub(crate) fn list_columns(widest: u16) -> u16 {
    widest
        .max(LIST_MIN_COLUMNS)
        .saturating_add(LABEL_PAD + MARK_COLUMNS)
}

/// The width of a plate fitted to a list of `list` columns.
///
/// The preview beside the list takes one and a half times the list, and at
/// least `preview_min` columns. The plate holds the list, the separator column,
/// the preview, and its two border columns.
pub(crate) fn fitted_plate_width(list: u16, preview_min: u16) -> u16 {
    let list = u32::from(list);
    let preview = (list * 3).div_ceil(2).max(u32::from(preview_min));
    u16::try_from(list + 1 + preview + 2).unwrap_or(u16::MAX)
}

/// The fewest columns a fitted preview takes.
///
/// `scaled` is the pixel width of a Preview Scale cell and of a grid cell where
/// the preview draws at Preview Scale, or `None` where it draws at the grid
/// size. A grid preview keeps [`PREVIEW_TEXT_COLUMNS`]. A scaled preview keeps
/// that many scaled cells of text and the scaled cells of its ruled number
/// column, and fits them into grid columns.
pub(crate) fn preview_min_columns(scaled: Option<(u16, u16)>) -> u16 {
    let Some((scaled_width, grid_width)) = scaled else {
        return PREVIEW_TEXT_COLUMNS;
    };
    let cells = PREVIEW_TEXT_COLUMNS + PREVIEW_NUMBER_DIGITS + PREVIEW_RULE_CELLS;
    let pixels = u32::from(cells) * u32::from(scaled_width);
    u16::try_from(pixels.div_ceil(u32::from(grid_width.max(1)))).unwrap_or(u16::MAX)
}

/// The most chars of a label among the `list_rows` results that a list shows
/// from [`window_start`].
///
/// A fitted list sizes its plate from this. Below the plate's cap no label
/// wraps, so the results from the window's start are the rows on show.
pub(crate) fn widest_label(rows: &PathRows<'_>, list_rows: u16) -> u16 {
    let first = window_start(rows.picklist.selected, usize::from(list_rows));
    let mut label = String::new();
    let widest = (first..first.saturating_add(usize::from(list_rows)))
        .map_while(|result| {
            rows.label(result, &mut label)?;
            Some(label.chars().count())
        })
        .max()
        .unwrap_or(0);
    u16::try_from(widest).unwrap_or(u16::MAX)
}

/// Split a fitted finder body, the `width` by `height` cells from `(x, y)`,
/// into its list and its preview.
///
/// A body under [`FITTED_SPLIT_MIN_COLUMNS`] shows the list alone. Otherwise a
/// dragged `split` share, in [`SHARE_SCALE`] units, sets the list as that share
/// of the body, and each pane keeps [`MIN_PANE_COLUMNS`]. With no share the
/// list takes its fitted `list` columns, as [`list_columns`] gives them, and
/// the preview keeps `preview_min` columns. In a body too narrow for both, the
/// preview's minimum yields before the list's own minimum does. The preview
/// takes the cells past one separator column.
pub(crate) fn fitted_split(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    list: u16,
    preview_min: u16,
    split: Option<u16>,
) -> (Rect, Option<Rect>) {
    if width < FITTED_SPLIT_MIN_COLUMNS {
        return (Rect::new(x, y, width, height), None);
    }

    let list = match split {
        Some(share) => share_of(width, share).clamp(MIN_PANE_COLUMNS, width - 1 - MIN_PANE_COLUMNS),
        None => {
            let floor = preview_min.min(width - 1 - list_columns(0));
            list.min(width - 1 - floor)
        },
    };
    (
        Rect::new(x, y, list, height),
        Some(Rect::new(x + list + 1, y, width - list - 1, height)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        test_support::{match_theme, rounded_row_boxes, rounded_row_theme, row_boxes, ROW_SELECT},
        *,
    };
    use crate::{
        input_view::SubmitTarget,
        picker::{PreviewSource, INDEXED_ROWS},
        test_fixture,
    };
    use ratatui::{buffer::Cell, style::Color};
    use std::{path::PathBuf, sync::Arc};
    use stoatty_protocol::command::{
        decode_stream, encode_bar, BarCommand, BorderStyle, Command, Edge, PanelCommand,
        PanelShadow, TextRunCommand,
    };

    /// A path whose 42-char label wraps at its slashes onto four lines of a
    /// list 20 cells wide, `very/deeply/`, `nested/dir/`, `module/`, and
    /// `file_name.rs`.
    const LONG_PATH: &str = "/r/very/deeply/nested/dir/module/file_name.rs";

    /// The header a finder-like modal paints, its chip at the row's end.
    const FINDER_HEADER: FilterHeader<'static> = FilterHeader {
        label: "Files",
        prompt: FILTER_PROMPT,
        count: Some((14, 890)),
        chips: &[("S-Tab \u{00B7} git only", true)],
    };

    /// A bar a paint draws, as `(x, y, width, height, color)` in sixteenths of
    /// a cell from the paint's area.
    type BarDraw = (i16, i16, u16, u16, [u8; 3]);

    /// What a preview's paint marks: the rows whose every cell takes the list's
    /// selected background, each bar it draws, and the color of each number's
    /// run, top down.
    type PreviewMarks = (Vec<u16>, Vec<BarDraw>, Vec<[u8; 3]>);

    /// The text of a finder-like header painted `width` columns wide, and the
    /// rect it leaves the query.
    fn header_row(width: u16) -> (String, Rect) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let query = paint_header_row(&mut buf, area, &FINDER_HEADER, &Theme::empty(), &mut scene);
        (test_support::row_text(&buf, 0), query)
    }

    /// Under a live scene the chip's box is a chip panel over its 18 cells, and
    /// the cells take the chip's text color over the row's own background.
    #[test]
    fn a_live_header_draws_its_chip_box_as_a_panel() {
        let theme = {
            let (config, _) =
                stoat_config::parse(r##"theme t { ui.chip = { fg: "#010203", bg: "#040506" }; }"##);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &FINDER_HEADER, &theme, &mut scene);

        let cells: Vec<_> = (42..60).map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg)).collect();
        assert_eq!(
            (cells, decode_stream(scene.bytes())),
            (
                vec![(Color::Rgb(1, 2, 3), Color::Reset); 18],
                vec![Command::Panel(PanelCommand {
                    top: 0,
                    left: 42,
                    width: 18,
                    height: 1,
                    style: BorderStyle::None_,
                    border: [4, 5, 6],
                    corner_radius: 4,
                    fill: Some([4, 5, 6]),
                    shadow: PanelShadow::None_,
                    edges: Edge::symmetric(0, 1),
                    corners: 0,
                    fillets: 0,
                    fillet_radius: 2,
                    hatch: None,
                    above_pools: false,
                    backdrop: false,
                    anchor: None,
                })],
            ),
        );
    }

    #[test]
    fn the_header_sets_its_count_and_chip_flush_right() {
        assert_eq!(
            header_row(60),
            (
                format!(
                    "Files \u{203A} {}14 of 890  S-Tab \u{00B7} git only ",
                    " ".repeat(24)
                ),
                Rect::new(8, 0, 23, 1),
            ),
            "the chip ends the row, and the count stands one column left of it",
        );
    }

    /// A scope header names every scope, right to left and one column apart.
    /// The active one is a chip panel over its cells, and the other is plain
    /// muted text with no box.
    #[test]
    fn a_scope_header_boxes_the_active_chip_and_leaves_the_other_plain() {
        let theme = {
            let src = r##"theme t {
                ui.chip = { fg: "#010203", bg: "#040506" };
                ui.text.muted.fg = "#070809";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = FilterHeader {
            label: "help",
            prompt: FILTER_PROMPT,
            count: Some((14, 214)),
            chips: &[("applicable", true), ("all", false)],
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &header, &theme, &mut scene);

        let panels: Vec<_> = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((panel.left, panel.width)),
                _ => None,
            })
            .collect();
        assert_eq!(
            (
                test_support::row_text(&buf, 0)
                    .chars()
                    .skip(32)
                    .collect::<String>(),
                panels,
                [43, 56].map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg)),
            ),
            (
                "14 of 214  applicable   all ".to_owned(),
                vec![(42, 12)],
                [
                    (Color::Rgb(1, 2, 3), Color::Reset),
                    (Color::Rgb(7, 8, 9), Color::Reset)
                ],
            ),
        );
    }

    /// A count of no shown entries reads its zero in `diff.deleted`'s
    /// foreground over the muted background, and the rest of the count muted.
    #[test]
    fn a_zero_count_reads_its_zero_in_the_deleted_color() {
        let theme = {
            let src = r##"theme t {
                diff.deleted = { fg: "#010203", bg: "#040506" };
                ui.text.muted = { fg: "#070809", bg: "#0a0b0c" };
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = FilterHeader {
            label: "Files",
            prompt: FILTER_PROMPT,
            count: Some((0, 214)),
            chips: &[],
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &header, &theme, &mut scene);

        let muted = (Color::Rgb(7, 8, 9), Color::Rgb(10, 11, 12));
        assert_eq!(
            (
                test_support::row_text(&buf, 0),
                (32..40)
                    .map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg))
                    .collect::<Vec<_>>(),
            ),
            (
                format!("Files \u{203A} {}0 of 214", " ".repeat(24)),
                [
                    vec![(Color::Rgb(1, 2, 3), Color::Rgb(10, 11, 12))],
                    vec![muted; 7],
                ]
                .concat(),
            ),
        );
    }

    /// The query's width tells which words stayed. At 49 columns both fit with
    /// the query at its floor, at 48 only the count does, at 30 the count fits
    /// at the floor again, and at 29 neither does.
    #[test]
    fn a_narrow_header_drops_its_chip_and_then_its_count() {
        assert_eq!(
            [49, 48, 30, 29].map(|width| header_row(width).1.width),
            [12, 30, 12, 21],
            "the chip goes first, then the count, so the query keeps its floor",
        );
    }

    const PLATE_GROUND: [u8; 3] = [1, 1, 1];
    const PLATE_LABEL: [u8; 3] = [3, 3, 3];
    const PLATE_TEXT: [u8; 3] = [4, 4, 4];
    const PLATE_MUTED: [u8; 3] = [5, 5, 5];
    const PLATE_CHIP_TEXT: [u8; 3] = [6, 6, 6];
    const PLATE_CHIP_FILL: [u8; 3] = [7, 7, 7];
    const PLATE_DELETED: [u8; 3] = [8, 8, 8];

    /// A theme of the `PLATE_` colors, whose plate needs a scene that reads
    /// nudges.
    fn plate_header_theme() -> Theme {
        let src = r##"theme t {
            ui.surface.ground.bg = "#010101";
            ui.background.bg = "#020202";
            ui.modal.label = { fg: "#030303", modifiers: [bold] };
            ui.text.fg = "#040404";
            ui.text.muted.fg = "#050505";
            ui.chip = { fg: "#060606", bg: "#070707" };
            diff.deleted.fg = "#080808";
            ui.border.inactive.fg = "#090909";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The buffer and the scene of `header` painted into the inner rect
    /// (2, 1, 60, 4) over an input that holds `ab`, under
    /// [`plate_header_theme`], by a scene that reads nudges where `nudges`
    /// holds.
    ///
    /// Every cell of the 64 by 6 buffer holds `x` before the paint.
    fn painted_header(
        header: &FilterHeader<'_>,
        nudges: bool,
    ) -> (Buffer, stoat_widgets::ApcScene) {
        let theme = plate_header_theme();
        let mut h = crate::Stoat::test();
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let input = InputView::create(ws, executor, SubmitTarget::FileFinder, "ab", "insert");

        let mut buf = Buffer::filled(Rect::new(0, 0, 64, 6), Cell::new("x"));
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(nudges);
        filter_header(
            &mut buf,
            Rect::new(2, 1, 60, 4),
            header,
            &input,
            ws,
            &theme,
            &ResolvedChrome::resolve(&theme),
            &mut scene,
        );
        (buf, scene)
    }

    /// A plate header run of `text` at `col` sixteenths on the header row.
    fn plate_run(col: i16, text: &str, color: [u8; 3], bg: [u8; 3], bold: bool) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row: 16,
            scale: 256,
            color,
            bg: Some(bg),
            bold,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    /// The block caret's bar at `col` sixteenths on the header row, 1
    /// sixteenth under the row's top and 13 high in the text color.
    fn plate_caret_bar(col: i16) -> Command {
        Command::Bar(BarCommand {
            x: col,
            y: 17,
            width: 16,
            height: 13,
            color: PLATE_TEXT,
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        })
    }

    /// The rounded bar of an active chip, `width` sixteenths from `x` on the
    /// header row at `y`.
    fn plate_chip_bar(x: i16, y: i16, width: u16) -> Command {
        Command::Bar(BarCommand {
            x,
            y,
            width,
            height: 16,
            color: PLATE_CHIP_FILL,
            occludes: false,
            hatch: None,
            radius: 4,
            corners: 0,
        })
    }

    /// The commands of a scene, less the ends that close its runs.
    fn plate_commands(scene: &stoat_widgets::ApcScene) -> Vec<Command> {
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// Under the plate the header row is runs 1 px right and 7 px down of its
    /// cells, with no rule under it.
    ///
    /// The label leads in its bold color, and the prompt and the query follow
    /// 14 sixteenths apart. From the right, the row ends 4 sixteenths inside
    /// its end, and each chip takes its text with 11 sixteenths at each side, 4
    /// sixteenths from the next, so `all` stands last and the active
    /// `applicable` before it over a rounded bar of that span. The count ends
    /// 14 sixteenths left of that bar. The caret after `ab` covers a blank, so
    /// it draws last as its bar alone, 1 sixteenth under the row's top and 13
    /// high in the text color.
    #[test]
    fn a_plate_header_draws_its_parts_as_nudged_runs() {
        let header = FilterHeader {
            label: "Help",
            prompt: FILTER_PROMPT,
            count: Some((14, 890)),
            chips: &[("applicable", true), ("all", false)],
        };
        assert_eq!(
            plate_commands(&painted_header(&header, true).1),
            vec![
                Command::Nudge([1, 7]),
                plate_chip_bar(732, 16, 182),
                plate_run(32, "Help", PLATE_LABEL, PLATE_GROUND, true),
                plate_run(110, FILTER_PROMPT, PLATE_MUTED, PLATE_GROUND, false),
                plate_run(140, "ab", PLATE_TEXT, PLATE_GROUND, false),
                plate_run(574, "14 of 890", PLATE_MUTED, PLATE_GROUND, false),
                plate_run(743, "applicable", PLATE_CHIP_TEXT, PLATE_CHIP_FILL, false),
                plate_run(929, "all", PLATE_MUTED, PLATE_GROUND, false),
                plate_caret_bar(172),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// A plate header with no label starts at its prompt, and a count of no
    /// shown entries reads its zero in the deleted color and the rest muted.
    #[test]
    fn a_plate_header_reads_a_zero_count_in_the_deleted_color() {
        let header = FilterHeader {
            label: "",
            prompt: FILTER_PROMPT,
            count: Some((0, 214)),
            chips: &[],
        };
        assert_eq!(
            plate_commands(&painted_header(&header, true).1),
            vec![
                Command::Nudge([1, 7]),
                plate_run(32, FILTER_PROMPT, PLATE_MUTED, PLATE_GROUND, false),
                plate_run(62, "ab", PLATE_TEXT, PLATE_GROUND, false),
                plate_run(860, "0", PLATE_DELETED, PLATE_GROUND, false),
                plate_run(876, " of 214", PLATE_MUTED, PLATE_GROUND, false),
                plate_caret_bar(94),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// A caret over a char draws its bar at the char's col, and the char over
    /// the bar in the ground with no background of its own. The chars after
    /// the caret draw as their own run one char further right.
    #[test]
    fn a_caret_over_a_char_draws_that_char_in_the_ground() {
        let header = FilterHeader {
            label: "",
            prompt: FILTER_PROMPT,
            count: None,
            chips: &[],
        };
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(true);
        paint_plate_header(
            &mut Buffer::empty(Rect::new(0, 0, 64, 6)),
            Rect::new(2, 1, 60, 4),
            &header,
            ("ab".to_owned(), 0),
            PLATE_GROUND,
            &plate_header_theme(),
            &mut scene,
        );

        assert_eq!(
            plate_commands(&scene),
            vec![
                Command::Nudge([1, 7]),
                plate_run(32, FILTER_PROMPT, PLATE_MUTED, PLATE_GROUND, false),
                plate_run(78, "b", PLATE_TEXT, PLATE_GROUND, false),
                plate_caret_bar(62),
                Command::TextRun(TextRunCommand {
                    col: 62,
                    row: 16,
                    scale: 256,
                    color: PLATE_GROUND,
                    bg: None,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                    text: "a".to_owned(),
                }),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// A label wider than the plate header's row clips at the row's end. The
    /// prompt, the query, the count, and the chip find no room after it.
    #[test]
    fn a_plate_header_clips_a_label_wider_than_its_row() {
        let label = "L".repeat(70);
        let header = FilterHeader {
            label: &label,
            prompt: FILTER_PROMPT,
            count: Some((1, 2)),
            chips: &[("all", true)],
        };
        assert_eq!(
            plate_commands(&painted_header(&header, true).1),
            vec![
                Command::Nudge([1, 7]),
                plate_run(32, &"L".repeat(60), PLATE_LABEL, PLATE_GROUND, true),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// Under the plate the header row's cells blank to the plate's ground, so
    /// no glyph stands under the runs. The cells around the row keep theirs.
    #[test]
    fn a_plate_header_blanks_its_row_to_the_ground() {
        let (buf, _) = painted_header(&FINDER_HEADER, true);
        let row: Vec<_> = (0..64)
            .map(|x| (buf[(x, 1)].symbol(), buf[(x, 1)].bg))
            .collect();
        assert_eq!(
            (row, buf[(10, 2)].symbol()),
            (
                [
                    vec![("x", Color::Reset); 2],
                    vec![(" ", Color::Rgb(1, 1, 1)); 60],
                    vec![("x", Color::Reset); 2],
                ]
                .concat(),
                "x",
            ),
        );
    }

    /// A scene that reads no nudges keeps the cell look, which rules the
    /// header's second row off from the body and nudges nothing.
    #[test]
    fn a_header_without_the_plate_rules_its_second_row() {
        let (_, scene) = painted_header(&FINDER_HEADER, false);
        let nudges = plate_commands(&scene)
            .into_iter()
            .filter(|command| matches!(command, Command::Nudge(_)))
            .count();
        assert_eq!((test_fixture::row_rules(&scene, 2), nudges), (1, 0));
    }

    /// The query's end tells which parts stayed. At 47 cells every part fits
    /// with the query past its floor, at 46 only the count does, at 30 the
    /// count fits again, and at 29 no part does.
    #[test]
    fn a_narrow_plate_header_drops_its_chips_and_then_its_count() {
        let chips = [("applicable", true), ("all", false)];
        assert_eq!(
            [47, 46, 30, 29].map(|width: u16| {
                plate_trailing(Some("14 of 890"), &chips, true, width * 16, 316, 256).query_end
            }),
            [320, 574, 318, 460],
        );
    }

    /// The query keeps its tail in view, and the caret too when it stands left
    /// of that tail. A caret past the end covers a space.
    #[test]
    fn the_plate_query_keeps_its_tail_and_its_caret_in_view() {
        assert_eq!(
            [
                query_window("abc", 3, 10),
                query_window("abc", 1, 10),
                query_window("abcdef", 6, 4),
                query_window("abcdef", 1, 4),
                query_window("abc", 3, 0),
            ],
            [
                ("abc", Some(" "), ""),
                ("a", Some("b"), "c"),
                ("def", Some(" "), ""),
                ("", Some("b"), "cde"),
                ("", None, ""),
            ],
        );
    }

    const PLATE_SHA: [u8; 3] = [10, 10, 10];

    /// A muted span and a sha span in their own colors, the meta that every
    /// title header test paints.
    fn title_meta() -> [(String, Style); 2] {
        [
            (
                "fake \u{b7} ".to_owned(),
                Style::default().fg(Color::Rgb(5, 5, 5)),
            ),
            ("c1".to_owned(), Style::default().fg(Color::Rgb(10, 10, 10))),
        ]
    }

    /// The buffer and the scene of a title header of `Commit`, `two`, `meta`,
    /// and the read-only chip, which [`plate_header`] paints into the inner
    /// rect (2, 1, `width`, 4) under a theme of the `PLATE_` colors, by a
    /// scene that reads nudges where `nudges` holds.
    fn painted_title_header(
        meta: &[(String, Style)],
        width: u16,
        nudges: bool,
    ) -> (Buffer, stoat_widgets::ApcScene) {
        let theme = {
            let src = r##"theme t {
                ui.surface.ground.bg = "#010101";
                ui.background.bg = "#020202";
                ui.modal.label = { fg: "#030303", modifiers: [bold] };
                ui.text.fg = "#040404";
                ui.text.muted.fg = "#050505";
                ui.preview.chip = { fg: "#060606", bg: "#070707" };
                ui.border.inactive.fg = "#090909";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = PlateHeader {
            label: "Commit",
            title: "two",
            meta,
            chip: Some("preview \u{b7} read-only"),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 4, 6));
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(nudges);
        plate_header(
            &mut buf,
            Rect::new(2, 1, width, 4),
            &header,
            &theme,
            &mut scene,
        );
        (buf, scene)
    }

    /// Under the plate a title header is runs 1 px right and 7 px down of its
    /// cells, which blank to the ground, with no rule under it.
    ///
    /// The label leads in its bold color, and the bold title follows 14
    /// sixteenths after it. From the right, the chip's run stands 11
    /// sixteenths inside a rounded bar that ends 4 sixteenths inside the row.
    /// The meta's spans end 14 sixteenths left of that bar, each in its own
    /// color.
    #[test]
    fn a_plate_title_header_draws_its_parts_as_nudged_runs() {
        let (buf, scene) = painted_title_header(&title_meta(), 60, true);
        let row: Vec<Color> = (0..64).map(|x| buf[(x, 1)].bg).collect();
        assert_eq!(
            (row, plate_commands(&scene)),
            (
                [
                    vec![Color::Reset; 2],
                    vec![Color::Rgb(1, 1, 1); 60],
                    vec![Color::Reset; 2],
                ]
                .concat(),
                vec![
                    Command::Nudge([1, 7]),
                    plate_chip_bar(662, 16, 326),
                    plate_run(32, "Commit", PLATE_LABEL, PLATE_GROUND, true),
                    plate_run(142, "two", PLATE_TEXT, PLATE_GROUND, true),
                    plate_run(504, "fake \u{b7} ", PLATE_MUTED, PLATE_GROUND, false),
                    plate_run(616, "c1", PLATE_SHA, PLATE_GROUND, false),
                    plate_run(
                        673,
                        "preview \u{b7} read-only",
                        PLATE_CHIP_TEXT,
                        PLATE_CHIP_FILL,
                        false,
                    ),
                    Command::Nudge([0, 0]),
                ],
            ),
        );
    }

    /// Without the plate a title header is cells. The label and the bold title
    /// lead. From the right, the chip's padded box ends the row, and the meta
    /// stands one column left of it, each span in its own color. A rule closes
    /// the header on the next row, and nothing nudges.
    #[test]
    fn a_cell_title_header_sets_its_meta_and_chip_flush_right() {
        let (buf, scene) = painted_title_header(&title_meta(), 60, false);
        let style = |x: u16| (buf[(x, 1)].fg, buf[(x, 1)].modifier);
        let rgb = |v: u8| Color::Rgb(v, v, v);
        let nudges = plate_commands(&scene)
            .into_iter()
            .filter(|command| matches!(command, Command::Nudge(_)))
            .count();
        assert_eq!(
            (
                test_support::row_text(&buf, 1),
                [2, 9, 31, 38, 42].map(style),
                (test_fixture::row_rules(&scene, 2), nudges),
            ),
            (
                format!(
                    "  Commit two{}fake \u{b7} c1  preview \u{b7} read-only   ",
                    " ".repeat(19)
                ),
                [
                    (rgb(3), Modifier::BOLD),
                    (rgb(4), Modifier::BOLD),
                    (rgb(5), Modifier::empty()),
                    (rgb(10), Modifier::empty()),
                    (rgb(6), Modifier::empty()),
                ],
                (1, 0),
            ),
        );
    }

    /// The title keeps its 12-column floor in both looks. At 51 columns every
    /// part fits, at 50 the chip drops, at 29 the meta still fits, and at 28 it
    /// drops too.
    #[test]
    fn a_narrow_title_header_drops_its_chip_and_then_its_meta() {
        let meta = title_meta();
        let kept = |width: u16, nudges: bool| {
            let (buf, scene) = painted_title_header(&meta, width, nudges);
            let runs: String = plate_commands(&scene)
                .into_iter()
                .filter_map(|command| match command {
                    Command::TextRun(run) => Some(run.text),
                    _ => None,
                })
                .collect();
            let drawn = test_support::row_text(&buf, 1) + &runs;
            (drawn.contains("read-only"), drawn.contains("c1"))
        };
        assert_eq!(
            [false, true].map(|nudges| [51, 50, 29, 28].map(|width| kept(width, nudges))),
            [[(true, true), (false, true), (false, true), (false, false)]; 2],
            "the chip goes first, then the meta, so the title keeps its floor",
        );
    }

    /// The hints every footer test paints.
    const FOOTER_HINTS: [(&str, &str); 2] = [("Enter", "open"), ("C-f/C-b", "page")];

    /// A theme whose key line colors are each their own.
    fn footer_theme() -> Theme {
        let src = r##"theme t {
            ui.surface.ground.bg = "#010101";
            ui.background.bg = "#020202";
            ui.key_label.fg = "#030303";
            ui.text.muted.fg = "#040404";
            ui.border.inactive.fg = "#070809";
            ui.modal.footer.bg = "#0a0b0c";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// A legend of a mark in `#050505` and a muted word.
    fn footer_legend() -> [(&'static str, Style); 2] {
        [
            ("\u{25cf}", Style::default().fg(Color::Rgb(5, 5, 5))),
            (" open", Style::default().fg(Color::Rgb(4, 4, 4))),
        ]
    }

    /// Each row of the cell look's `footer` painted `width` by `height` in a
    /// live scene, as its text and whether every cell takes the footer's
    /// surface, and the bar frames its rule sends.
    fn cell_footer(width: u16, height: u16, footer: &Footer<'_>) -> (Vec<(String, bool)>, Vec<u8>) {
        let theme = footer_theme();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        modal_footer(&mut buf, area, footer, None, &theme, &mut scene);

        let surface = theme.get(scope::UI_MODAL_FOOTER).bg;
        let rows = (0..height)
            .map(|y| {
                let filled = (0..width).all(|x| Some(buf[(x, y)].bg) == surface);
                (test_support::row_text(&buf, y), filled)
            })
            .collect();
        (rows, scene.buffer().clone())
    }

    /// The first row's text of the cell look's footer of [`FOOTER_HINTS`] and
    /// the one-span note `note`, painted `width` columns wide.
    fn cell_footer_line(width: u16, note: &str) -> String {
        let note = [(note, Style::default())];
        let footer = Footer {
            hints: &FOOTER_HINTS,
            note: &note,
            ..Footer::default()
        };
        cell_footer(width, 1, &footer).0[0].0.clone()
    }

    /// The bar frame of a footer's rule across `width` cells, one sixteenth
    /// tall along the top of its first row.
    fn rule_frame(width: u16) -> Vec<u8> {
        encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: width * 16,
            height: 1,
            color: [7, 8, 9],
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        })
    }

    /// The cell look rules its top edge with a hairline and draws no dots. Its
    /// hints run from one cell in, two cells apart, and a hint that does not
    /// fit stays out. A footer two rows tall fills both rows with its surface
    /// and paints its line in the first.
    #[test]
    fn the_cell_footer_paints_its_hints_under_a_rule() {
        let footer = Footer {
            hints: &FOOTER_HINTS,
            ..Footer::default()
        };
        assert_eq!(
            [cell_footer(40, 2, &footer), cell_footer(20, 1, &footer)],
            [
                (
                    vec![
                        (format!(" Enter open  C-f/C-b page{}", " ".repeat(15)), true),
                        (" ".repeat(40), true),
                    ],
                    rule_frame(40),
                ),
                (
                    vec![(format!(" Enter open{}", " ".repeat(9)), true)],
                    rule_frame(20),
                ),
            ],
        );
    }

    /// A note ends one cell from the right edge. A note that does not clear the
    /// hints by two cells is left out.
    #[test]
    fn the_footer_sets_its_note_right_of_the_hints() {
        assert_eq!(
            [
                cell_footer_line(40, "normal"),
                cell_footer_line(30, "normal")
            ],
            [
                format!(" Enter open  C-f/C-b page{}normal ", " ".repeat(8)),
                format!(" Enter open  C-f/C-b page{}", " ".repeat(5)),
            ],
        );
    }

    /// A legend ends one cell from the right edge in its own styles, and a
    /// hint that does not clear it by two cells stays out, so the legend keeps
    /// its place in a narrow footer.
    #[test]
    fn the_footer_sets_its_legend_flush_right() {
        let legend = footer_legend();
        let footer = Footer {
            hints: &FOOTER_HINTS,
            legend: &legend,
            ..Footer::default()
        };
        let line = |width: u16| {
            let theme = footer_theme();
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            let mut scene = stoat_widgets::ApcScene::new();
            modal_footer(&mut buf, area, &footer, None, &theme, &mut scene);
            (test_support::row_text(&buf, 0), buf[(width - 7, 0)].fg)
        };
        assert_eq!(
            [line(40), line(30)],
            [
                (
                    format!(" Enter open  C-f/C-b page{}\u{25cf} open ", " ".repeat(8)),
                    Color::Rgb(5, 5, 5),
                ),
                (
                    format!(" Enter open{}\u{25cf} open ", " ".repeat(12)),
                    Color::Rgb(5, 5, 5),
                ),
            ],
        );
    }

    /// The legend keeps its place over the hints, and the note shows only
    /// when it still clears them with the legend moved left of it.
    #[test]
    fn the_footer_note_yields_to_the_hints_and_the_legend() {
        let legend = [("L", Style::default())];
        let note = [("n", Style::default()), ("n", Style::default())];
        let footer = Footer {
            hints: &[("ab", "cd")],
            legend: &legend,
            note: &note,
        };
        let chars = |text: &str| text.chars().count() as u16;
        assert_eq!(
            [
                place_footer(&footer, 20, (2, 2), chars),
                place_footer(&footer, 10, (2, 2), chars),
            ],
            [
                FooterPlaces {
                    hints: vec![0],
                    legend: Some(15),
                    note: Some(18),
                },
                FooterPlaces {
                    hints: vec![0],
                    legend: Some(9),
                    note: None,
                },
            ],
        );
    }

    /// The groups of a plate's key line stand 18 sixteenths apart. At 15 cells
    /// a two-char note leaves the one-char legend only 14 sixteenths past the
    /// one hint, so the note stays out and the legend ends the line.
    #[test]
    fn a_plate_footer_keeps_its_groups_18_sixteenths_apart() {
        let theme = footer_theme();
        let legend = [("L", Style::default().fg(Color::Rgb(5, 5, 5)))];
        let note = [("nn", Style::default().fg(Color::Rgb(4, 4, 4)))];
        let footer = Footer {
            hints: &FOOTER_HINTS[..1],
            legend: &legend,
            note: &note,
        };
        let plate = PlateColors {
            ground: [1, 1, 1],
            surface: [2, 2, 2],
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
        let mut scene = stoat_widgets::ApcScene::new();
        let rect = Rect::new(2, 5, 15, 2);
        modal_footer(&mut buf, rect, &footer, Some(plate), &theme, &mut scene);

        let texts: Vec<(i16, String)> = plate_commands(&scene)
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [(32, "Enter"), (128, "open"), (256, "L")].map(|(col, text)| (col, text.to_owned())),
        );
    }

    /// A plate's note ends the line as one run per span, each in its span's
    /// color, the spans one after another.
    #[test]
    fn a_plate_footer_draws_each_note_span_in_its_color() {
        let theme = footer_theme();
        let note = [
            ("a", Style::default().fg(Color::Rgb(5, 5, 5))),
            ("bc", Style::default().fg(Color::Rgb(6, 6, 6))),
        ];
        let footer = Footer {
            hints: &FOOTER_HINTS[..1],
            note: &note,
            ..Footer::default()
        };
        let plate = PlateColors {
            ground: [1, 1, 1],
            surface: [2, 2, 2],
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 44, 8));
        let mut scene = stoat_widgets::ApcScene::new();
        let rect = Rect::new(2, 5, 40, 2);
        modal_footer(&mut buf, rect, &footer, Some(plate), &theme, &mut scene);

        let runs: Vec<(i16, String, [u8; 3])> = plate_commands(&scene)
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text, run.color)),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs,
            [
                (32, "Enter", [3, 3, 3]),
                (128, "open", [4, 4, 4]),
                (624, "a", [5, 5, 5]),
                (640, "bc", [6, 6, 6]),
            ]
            .map(|(col, text, color)| (col, text.to_owned(), color)),
        );
    }

    /// Under the plate the key line is runs 1 px right and 7 px down of its
    /// first row, on the plate's ground, with no rule and no fill.
    ///
    /// The row's cells blank to the ground, and the row under it keeps its
    /// cells. The hints stand 25 sixteenths apart, each key in the key color
    /// and its word one space on in the muted color. The legend ends the line
    /// in its own colors.
    #[test]
    fn a_plate_footer_draws_its_line_as_nudged_runs_on_the_ground() {
        let theme = footer_theme();
        let legend = footer_legend();
        let footer = Footer {
            hints: &FOOTER_HINTS,
            legend: &legend,
            ..Footer::default()
        };
        let plate = PlateColors {
            ground: [1, 1, 1],
            surface: [2, 2, 2],
        };
        let rect = Rect::new(2, 5, 40, 2);
        let mut buf = Buffer::filled(Rect::new(0, 0, 44, 8), Cell::new("x"));
        let mut scene = stoat_widgets::ApcScene::new();
        modal_footer(&mut buf, rect, &footer, Some(plate), &theme, &mut scene);

        let run = |col: i16, text: &str, color: [u8; 3]| {
            Command::TextRun(TextRunCommand {
                col,
                row: 80,
                scale: 256,
                color,
                bg: Some([1, 1, 1]),
                bold: false,
                italic: false,
                follow: 0,
                anchor: None,
                text: text.to_owned(),
            })
        };
        let cells = |y: u16| {
            (2..42)
                .map(|x| (buf[(x, y)].symbol().to_owned(), buf[(x, y)].bg))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (plate_commands(&scene), cells(5), cells(6)),
            (
                vec![
                    Command::Nudge([1, 7]),
                    run(32, "Enter", [3, 3, 3]),
                    run(128, "open", [4, 4, 4]),
                    run(217, "C-f/C-b", [3, 3, 3]),
                    run(345, "page", [4, 4, 4]),
                    run(576, "\u{25cf}", [5, 5, 5]),
                    run(592, " open", [4, 4, 4]),
                    Command::Nudge([0, 0]),
                ],
                vec![(" ".to_owned(), Color::Rgb(1, 1, 1)); 40],
                vec![("x".to_owned(), Color::Reset); 40],
            ),
        );
    }

    #[test]
    fn the_window_holds_the_selection_on_the_last_row() {
        assert_eq!(window_start(3, 12), 0, "a selection on screen scrolls not");
        assert_eq!(window_start(14, 12), 3, "one past it scrolls just enough");
        assert_eq!(
            window_start(0, 0),
            0,
            "and a list with no room to paint has nowhere to scroll to"
        );
    }

    /// The share is what the drag-resizable separator writes, so the layouts it
    /// feeds have to move with it, and an untouched modal has to land exactly
    /// where it landed before the share was a parameter at all.
    #[test]
    fn the_list_takes_the_share_it_is_given() {
        let split = |share| split_list_preview(0, 0, 100, 10, 80, 24, share);

        assert_eq!(
            split(DEFAULT_LIST_SHARE),
            (Rect::new(0, 0, 40, 10), Some(Rect::new(41, 0, 59, 10))),
            "the default share splits the body where it always has"
        );
        assert_eq!(
            split(6_000),
            (Rect::new(0, 0, 60, 10), Some(Rect::new(61, 0, 39, 10))),
            "a wider share moves the boundary and the preview takes the rest"
        );
        assert_eq!(
            split(500),
            (Rect::new(0, 0, 24, 10), Some(Rect::new(25, 0, 75, 10))),
            "a share under the minimum still leaves the list its floor"
        );
    }

    /// A body too narrow for two panes has no separator to drag, so the share is
    /// moot and the list keeps the full width whatever it says.
    #[test]
    fn a_narrow_body_ignores_the_share_and_stays_list_only() {
        assert_eq!(
            split_list_preview(0, 0, 79, 10, 80, 24, 6_000),
            (Rect::new(0, 0, 79, 10), None),
            "below the wide threshold the list takes everything"
        );
    }

    /// A fitted list holds its widest label, at least 30 columns, and its pad
    /// and mark cells. The plate holds the list, the separator, the larger of
    /// one and a half lists and the preview's minimum, and two borders. A
    /// scaled preview's minimum fits 100 text cells and the ruled number column
    /// of 5 px cells into 9 px grid columns.
    #[test]
    fn a_fitted_plate_holds_its_list_and_the_larger_preview() {
        assert_eq!(
            (
                [list_columns(0), list_columns(75)],
                [(33, 59), (47, 59), (33, 100)].map(|(list, min)| fitted_plate_width(list, min)),
                [preview_min_columns(None), preview_min_columns(Some((5, 9)))],
            ),
            ([33, 78], [95, 121, 136], [100, 59]),
        );
    }

    /// With no share the list takes its fitted columns, up to the room that the
    /// preview's minimum leaves, and the preview takes the rest past the
    /// separator. In a body too narrow for both minimums the preview yields to
    /// a 30-column list. A share sets the list as that share of the body, and a
    /// body under 80 columns shows no preview.
    #[test]
    fn a_fitted_split_gives_the_list_its_label_and_the_preview_the_rest() {
        let body = |width, list, min, split| fitted_split(0, 0, width, 27, list, min, split);
        let panes = |list, preview: Option<(u16, u16)>| {
            (
                Rect::new(0, 0, list, 27),
                preview.map(|(x, width)| Rect::new(x, 0, width, 27)),
            )
        };

        assert_eq!(
            [
                body(93, 33, 59, None),
                body(156, 164, 59, None),
                body(156, 78, 59, None),
                body(116, 60, 100, None),
                body(156, 96, 59, Some(6_000)),
                body(79, 33, 59, None),
            ],
            [
                panes(33, Some((34, 59))),
                panes(96, Some((97, 59))),
                panes(78, Some((79, 77))),
                panes(33, Some((34, 82))),
                panes(93, Some((94, 62))),
                panes(79, None),
            ],
        );
    }

    fn row_text(buf: &Buffer, row: u16, area: Rect) -> String {
        (area.x..area.x + area.width)
            .map(|c| buf[(c, row)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// A picklist over `base` with every row filtered in and no row selected, so
    /// the painter uses the plain row style.
    fn list_of(base: Vec<PathBuf>, match_indices: Vec<Vec<u32>>) -> PickList {
        PickList {
            filtered: (0..base.len()).collect(),
            base,
            match_indices,
            selected: usize::MAX,
            ..PickList::default()
        }
    }

    /// The results of `list` under the root `/r`, with no prefix.
    fn rows_of<'a>(list: &'a PickList, home: Option<&'a Path>) -> PathRows<'a> {
        PathRows {
            picklist: list,
            git_root: Path::new("/r"),
            home,
            prefix: "",
        }
    }

    /// A paint that starts on the first row of result `result`.
    fn from_result(result: usize) -> RowStart {
        RowStart { result, skip: 0 }
    }

    /// The cells of `area` in `buf` that hold the match color of
    /// [`match_theme`], as `(column, row)`, row by row.
    fn lit_cells(buf: &Buffer, area: Rect) -> Vec<(u16, u16)> {
        let match_fg = Color::Rgb(255, 0, 0);
        (area.top()..area.bottom())
            .flat_map(|row| (area.left()..area.right()).map(move |col| (col, row)))
            .filter(|&(col, row)| buf[(col, row)].fg == match_fg)
            .collect()
    }

    /// Paint `list` under `theme` into `area` from `start`, with no selection
    /// box and the default row style, and return the cells.
    fn painted_list(list: &PickList, area: Rect, start: RowStart, theme: &Theme) -> Buffer {
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &rows_of(list, None),
            RowMarks::Modified(&[]),
            area,
            start,
            theme,
            Style::default(),
            false,
            None,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );
        buf
    }

    /// The text of each row of `area` in `buf`, with trailing blanks trimmed.
    fn row_texts(buf: &Buffer, area: Rect) -> Vec<String> {
        (area.top()..area.bottom())
            .map(|row| row_text(buf, row, area).trim_end().to_owned())
            .collect()
    }

    /// A 42-char path wraps at its slashes onto four rows of a list 20 cells
    /// wide. Each later line starts three cells in with the Wrap Mark and one
    /// blank, and no row draws an ellipsis.
    #[test]
    fn a_long_path_wraps_under_the_wrap_mark() {
        let list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 4);
        let buf = painted_list(&list, area, from_result(0), &Theme::empty());

        assert_eq!(
            row_texts(&buf, area),
            [
                " very/deeply/",
                "   \u{21b3} nested/dir/",
                "   \u{21b3} module/",
                "   \u{21b3} file_name.rs",
            ],
        );
    }

    /// A relative entry is a label, such as a branch name of the palette's
    /// argument list, and not a path, so it breaks at the last cell of each
    /// line rather than at its slashes. A match keeps its highlight on the
    /// line it falls on.
    #[test]
    fn a_long_label_that_is_no_path_wraps_at_the_last_cell() {
        let label = "feature/a-long-branch-name-for-the-cut";
        let list = list_of(vec![PathBuf::from(label)], vec![vec![0, 18]]);
        let area = Rect::new(0, 0, 20, 3);
        let buf = painted_list(&list, area, from_result(0), &match_theme());

        assert_eq!(
            (row_texts(&buf, area), lit_cells(&buf, area)),
            (
                vec![
                    " feature/a-long-br".to_owned(),
                    "   \u{21b3} anch-name-for".to_owned(),
                    "   \u{21b3} -the-cut".to_owned(),
                ],
                vec![(1, 0), (6, 1)],
            ),
        );
    }

    /// A row outside the repo abbreviates against the home the caller supplies,
    /// which is what keeps the row paint from resolving one itself.
    #[test]
    fn a_row_outside_the_repo_abbreviates_against_the_given_home() {
        let list = list_of(vec![PathBuf::from("/fixture-home/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 24, 1);

        let paint = |home: Option<&Path>| {
            let mut buf = Buffer::empty(area);
            paint_path_rows(
                &rows_of(&list, home),
                RowMarks::Modified(&[]),
                area,
                from_result(0),
                &Theme::empty(),
                Style::default(),
                false,
                None,
                &mut buf,
                &mut stoat_widgets::ApcScene::new(),
            );
            row_text(&buf, 0, area).trim().to_string()
        };

        assert_eq!(paint(Some(Path::new("/fixture-home"))), "~/a.rs");
        assert_eq!(
            paint(None),
            "/fixture-home/a.rs",
            "and stays whole with no home to measure against"
        );
    }

    /// A path that fits its row takes that one row, whole, and leaves the row
    /// under it to the next result.
    #[test]
    fn a_path_that_fits_takes_one_row() {
        let list = list_of(vec![PathBuf::from("/r/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 2);
        let buf = painted_list(&list, area, from_result(0), &Theme::empty());

        assert_eq!(row_texts(&buf, area), [" a.rs", ""]);
    }

    /// A match keeps its highlight on whichever line of a wrapped path it falls
    /// on. Offset 0 lights the first cell of the first line, and offset 30, the
    /// `f` of `file_name`, lights the first cell of the fourth.
    #[test]
    fn a_match_keeps_its_highlight_on_a_continuation_line() {
        let list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![0, 30]]);
        let area = Rect::new(0, 0, 20, 4);
        let buf = painted_list(&list, area, from_result(0), &match_theme());

        assert_eq!(lit_cells(&buf, area), [(1, 0), (5, 3)]);
    }

    #[test]
    fn a_row_past_the_indexed_block_still_highlights() {
        let git_root = Path::new("/r");
        let base: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("/r/mod_{i:04}_alpha.rs")))
            .collect();

        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter("alpha", git_root);

        let deep = 550;
        assert!(
            deep >= list.indexed && deep < list.filtered.len(),
            "the painted row has to fall past the eagerly indexed block"
        );

        let area = Rect::new(0, 0, 40, 1);
        let buf = painted_list(&list, area, from_result(deep), &match_theme());

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .count();
        assert_eq!(
            highlighted, 5,
            "the query's five characters highlight on a row whose offsets were derived at paint"
        );
    }

    #[test]
    fn an_anchored_listing_highlights_a_row_past_the_stored_block() {
        let git_root = Path::new("/r");
        let base: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("/r/docs/page_{i:04}.md")))
            .collect();

        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter("./docs", git_root);
        assert_eq!(
            list.match_indices.len(),
            INDEXED_ROWS,
            "the listing stores highlights for the leading block only"
        );

        let area = Rect::new(0, 0, 40, 1);
        let buf = painted_list(&list, area, from_result(550), &match_theme());

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![1, 2, 3, 4],
            "the anchor's four characters highlight on a row past the block"
        );
    }

    #[test]
    fn unselected_rows_take_the_caller_style() {
        let mut list = list_of(
            vec![PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs")],
            vec![vec![], vec![]],
        );
        list.selected = 1;
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        let muted = Color::Rgb(1, 2, 3);
        paint_path_rows(
            &rows_of(&list, None),
            RowMarks::Modified(&[]),
            area,
            from_result(0),
            &Theme::empty(),
            Style::default().fg(muted),
            false,
            None,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let fg = |row: u16| -> Vec<Color> {
            (area.x..area.x + area.width)
                .map(|c| buf[(c, row)].fg)
                .collect()
        };
        assert_eq!(
            (fg(0), fg(1)),
            (vec![muted; 8], vec![Color::Reset; 8]),
            "the plain row paints in the caller's style and the selected row in the theme's",
        );
    }

    /// The row of an unsaved buffer ends in a round bar in the `ui.list.dirty`
    /// foreground under a live scene. The bar straddles the left edge of the
    /// row's last cell, and the cell stays blank.
    #[test]
    fn a_dirty_row_ends_in_a_round_bar_under_a_live_scene() {
        let (commands, buf) = marked_rows(
            RowMarks::Dirty(&[PathBuf::from("/r/a.rs")]),
            &dirty_theme(),
            true,
        );

        assert_eq!(
            (commands, buf[(19, 0)].symbol()),
            (
                vec![Command::Bar(BarCommand {
                    x: 19 * 16 - 5,
                    y: 5,
                    width: 11,
                    height: 6,
                    color: [1, 2, 3],
                    occludes: false,
                    hatch: None,
                    radius: 3,
                    corners: 0,
                })],
                " ",
            ),
        );
    }

    /// A plain terminal shows the dot of an unsaved buffer as a glyph in the
    /// row's last cell, in the `ui.list.dirty` foreground.
    #[test]
    fn a_dirty_row_ends_in_a_dot_glyph_on_a_plain_terminal() {
        let (commands, buf) = marked_rows(
            RowMarks::Dirty(&[PathBuf::from("/r/a.rs")]),
            &dirty_theme(),
            false,
        );

        assert_eq!(
            (commands, buf[(19, 0)].symbol(), buf[(19, 0)].fg),
            (vec![], "\u{25cf}", Color::Rgb(1, 2, 3)),
        );
    }

    /// A theme that leaves `ui.list.dirty` unset draws no dot, as a bar or as a
    /// glyph.
    #[test]
    fn a_theme_without_the_dirty_scope_draws_no_dot() {
        let mark = |live| {
            let (commands, buf) = marked_rows(
                RowMarks::Dirty(&[PathBuf::from("/r/a.rs")]),
                &Theme::empty(),
                live,
            );
            (commands, buf[(19, 0)].symbol().to_owned())
        };

        assert_eq!(
            [mark(true), mark(false)],
            [(vec![], " ".to_owned()), (vec![], " ".to_owned())],
        );
    }

    /// The marks of modified files end the row in an `M` and draw no dot, even
    /// under a theme that sets `ui.list.dirty`.
    #[test]
    fn a_modified_row_keeps_its_mark_under_the_modified_marks() {
        let (commands, buf) = marked_rows(
            RowMarks::Modified(&[PathBuf::from("/r/a.rs")]),
            &dirty_theme(),
            true,
        );

        assert_eq!((commands, buf[(19, 0)].symbol()), (vec![], "M"));
    }

    /// A result that wraps ends its first line in the dot, and its later lines
    /// end blank.
    #[test]
    fn a_wrapped_dirty_row_ends_its_first_line_in_the_dot() {
        let (_, buf) = marked_rows(
            RowMarks::Dirty(&[PathBuf::from(LONG_PATH)]),
            &dirty_theme(),
            false,
        );

        assert_eq!(
            row_texts(&buf, buf.area),
            [
                " very/deeply/      \u{25cf}",
                "   \u{21b3} nested/dir/",
                "   \u{21b3} module/",
                "   \u{21b3} file_name.rs",
            ],
        );
    }

    /// A theme whose `ui.list.dirty` foreground is `#010203`.
    fn dirty_theme() -> Theme {
        theme_of(r##"ui.list.dirty.fg = "#010203";"##)
    }

    /// Paint the paths that `marks` holds as the unselected rows of a list 20
    /// cells wide and 4 rows tall, under `theme` and a scene that is `live` or
    /// not.
    ///
    /// Returns the commands the scene holds and the painted cells.
    fn marked_rows(marks: RowMarks<'_>, theme: &Theme, live: bool) -> (Vec<Command>, Buffer) {
        let (RowMarks::Modified(paths) | RowMarks::Dirty(paths)) = marks;
        let list = list_of(paths.to_vec(), vec![vec![]; paths.len()]);
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(live);
        paint_path_rows(
            &rows_of(&list, None),
            marks,
            area,
            from_result(0),
            theme,
            Style::default(),
            false,
            None,
            &mut buf,
            &mut scene,
        );
        (decode_stream(scene.bytes()), buf)
    }

    /// A path row starts its label one cell in, with no tag column before it.
    /// The directory reads muted, the name bold, and the extension bold in the
    /// tag color of the file type.
    #[test]
    fn a_path_row_reads_its_directory_muted_its_name_bold_and_its_extension_in_the_tag_color() {
        let theme = theme_of(r##"ui.text.muted.fg = "#010203"; ui.tag.rust.fg = "#040506";"##);
        let buf = painted_row("/r/src/a.rs", vec![], 12, &theme);
        let (muted, tag) = (Color::Rgb(1, 2, 3), Color::Rgb(4, 5, 6));

        assert_eq!(
            (
                row_text(&buf, 0, buf.area),
                (1..9).map(|col| fg_and_bold(&buf, col)).collect()
            ),
            (
                " src/a.rs   ".to_owned(),
                vec![
                    (muted, false),
                    (muted, false),
                    (muted, false),
                    (muted, false),
                    (Color::Reset, true),
                    (tag, true),
                    (tag, true),
                    (tag, true),
                ],
            ),
        );
    }

    /// A C file's extension takes the C tag's color, bold as the name is.
    #[test]
    fn a_c_row_colors_its_extension() {
        let theme = theme_of(r##"ui.tag.c.fg = "#010203";"##);
        let buf = painted_row("/r/a.c", vec![], 10, &theme);
        let tag = Color::Rgb(1, 2, 3);

        assert_eq!(
            (1..4).map(|col| fg_and_bold(&buf, col)).collect::<Vec<_>>(),
            [(Color::Reset, true), (tag, true), (tag, true)]
        );
    }

    /// A gzip name takes the tag color on both of its extensions, the ones that
    /// `chrome::tagged_name` splits off, and its stem keeps the name's style.
    #[test]
    fn a_gzip_name_colors_both_of_its_extensions() {
        let theme = theme_of(r##"ui.tag.markdown.fg = "#040506";"##);
        let buf = painted_row("/r/CHANGELOG.md.gz", vec![], 20, &theme);
        let fgs: Vec<Color> = (1..16).map(|col| buf[(col, 0)].fg).collect();

        assert_eq!(
            fgs,
            [vec![Color::Reset; 9], vec![Color::Rgb(4, 5, 6); 6]].concat()
        );
    }

    /// A matched char of the extension reads in the match color, which outranks
    /// the tag color the rest of the extension takes.
    #[test]
    fn a_match_outranks_the_extension_color() {
        let theme = theme_of(r##"ui.search.match.fg = "#ff0000"; ui.tag.rust.fg = "#040506";"##);
        let buf = painted_row("/r/a.rs", vec![2], 12, &theme);
        let fgs: Vec<Color> = (1..5).map(|col| buf[(col, 0)].fg).collect();
        let tag = Color::Rgb(4, 5, 6);

        assert_eq!(fgs, [Color::Reset, tag, Color::Rgb(255, 0, 0), tag]);
    }

    /// A relative entry is a label, such as a branch name, and not a path, so
    /// it keeps the row's style whole: no muted directory, no bold name, and no
    /// tag color, even when it ends in a tagged extension.
    #[test]
    fn a_label_that_is_no_path_keeps_the_row_style() {
        let theme = theme_of(r##"ui.text.muted.fg = "#010203"; ui.tag.rust.fg = "#040506";"##);
        let buf = painted_row("feature/x.rs", vec![], 16, &theme);
        let cells: Vec<(Color, bool)> = (1..13).map(|col| fg_and_bold(&buf, col)).collect();

        assert_eq!(
            (row_text(&buf, 0, buf.area).trim().to_owned(), cells),
            ("feature/x.rs".to_owned(), vec![(Color::Reset, false); 12]),
        );
    }

    /// A theme `t` that sets only the scopes `body` assigns.
    fn theme_of(body: &str) -> Theme {
        let (config, _) = stoat_config::parse(&format!("theme t {{ {body} }}"));
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// Paint `path`, unselected and with its matches at `indices`, as the one
    /// row of a list `width` cells wide under `theme`.
    fn painted_row(path: &str, indices: Vec<u32>, width: u16, theme: &Theme) -> Buffer {
        let list = list_of(vec![PathBuf::from(path)], vec![indices]);
        painted_list(&list, Rect::new(0, 0, width, 1), from_result(0), theme)
    }

    /// The foreground of cell `col` of `buf`'s first row, and whether it is
    /// bold.
    fn fg_and_bold(buf: &Buffer, col: u16) -> (Color, bool) {
        let cell = &buf[(col, 0)];
        (cell.fg, cell.modifier.contains(Modifier::BOLD))
    }

    /// The box asks for a fixed size rather than growing with its candidates,
    /// so a terminal too small to host it gets no picker rather than a
    /// squeezed one.
    #[test]
    fn layout_is_none_when_the_terminal_cannot_host_the_box() {
        assert!(
            target_picker_layout(Rect::new(0, 0, 120, 40), 0, 40).is_some(),
            "a full-size terminal hosts it"
        );
        assert_eq!(
            (
                target_picker_layout(Rect::new(0, 0, 20, 40), 0, 40).is_none(),
                target_picker_layout(Rect::new(0, 0, 120, 4), 0, 40).is_none(),
            ),
            (true, true),
            "too narrow and too short both refuse rather than painting a stub"
        );
    }

    /// The width follows the window, so a 200-column area gets a box 160
    /// wide, 80 percent of the area.
    #[test]
    fn a_wide_window_widens_the_picker_box() {
        let layout = target_picker_layout(Rect::new(0, 0, 200, 60), 0, 40)
            .expect("the area hosts the picker");
        assert_eq!(layout.modal, Rect::new(20, 12, 160, 36));
    }

    /// A live scene with a scaled cell lays the preview out at Preview Scale. A
    /// 60 by 30 area of 9 by 18 pixel cells holds 49 rows of 5 by 11 pixel
    /// cells, which the call returns, the editor's viewport takes, and the
    /// preview records. The area's own cells blank to the editor surface.
    #[test]
    fn a_scaled_preview_lays_out_at_the_scaled_grid_and_blanks_its_cells() {
        let area = Rect::new(4, 2, 60, 30);

        assert_eq!(
            painted_preview(area, Some([9, 18])),
            (
                49,
                Some(49),
                Some(ScaledPreview {
                    area,
                    rows: 49,
                    cols: 108
                }),
                true
            ),
        );
    }

    /// Without a scaled cell the preview keeps the grid. It lays out the area's
    /// 30 rows, records nothing, and paints its text into the area's cells.
    #[test]
    fn a_preview_with_no_scaled_cell_paints_its_cells() {
        assert_eq!(
            painted_preview(Rect::new(4, 2, 60, 30), None),
            (30, Some(30), None, false),
        );
    }

    /// Paint a file finder's preview into `area` under a live scene whose scaled
    /// cell is `cell`. Answers the rows the call returns, the editor's viewport
    /// rows, what the preview records, and whether every cell of `area` is a
    /// blank on the editor surface.
    fn painted_preview(
        area: Rect,
        cell: Option<[u16; 2]>,
    ) -> (u16, Option<u32>, Option<ScaledPreview>, bool) {
        let mut h = crate::test_harness::TestHarness::with_size(120, 40);
        let root = PathBuf::from("/scaled-preview");
        let text: String = (1..=80).map(|n| format!("line {n}\n")).collect();
        h.fake_fs().insert_file(root.join("a.txt"), text.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();

        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let mut buf = Buffer::empty(Rect::new(0, 0, 70, 34));
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_scaled_cell(cell);
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let preview = &mut finder.active_core().preview;
        let ws = h.stoat.active_workspace_mut();
        let rows = render_picker_preview(preview, area, &theme, &chrome, ws, &mut buf, &mut scene);

        let viewport = ws
            .editors
            .get(preview.editor)
            .and_then(|editor| editor.viewport_rows);
        let blank = crate::render::surface_blank(&theme);
        let blanked = (area.top()..area.bottom())
            .all(|y| (area.left()..area.right()).all(|x| buf[(x, y)] == blank));
        (rows, viewport, preview.scaled, blanked)
    }

    /// At Preview Scale a 10-line file's column takes its four digit cells and
    /// two more. Each number's run ends at the digits' edge, 64 sixteenths, and
    /// one bar rules the column off 13 sixteenths into the fifth cell, 3
    /// sixteenths wide, down the four rows, in the divider color.
    #[test]
    fn a_scaled_preview_rules_off_its_numbers() {
        let (width, ends, bars, divider) = numbered_preview(true);

        assert_eq!(
            (width, ends, bars),
            (6, vec![64; 4], vec![(77, 0, 3, 64, divider)]),
        );
    }

    /// At the grid's cell the column keeps its one cell before the text, its
    /// numbers end 24 sixteenths short of the text, and nothing rules it off.
    #[test]
    fn a_grid_preview_keeps_its_column_and_draws_no_rule() {
        let (width, ends, bars, _) = numbered_preview(false);

        assert_eq!((width, ends, bars), (5, vec![56; 4], vec![]));
    }

    /// Number the first four rows of a finder's preview of a 10-line file under
    /// a live scene, `scaled` or at the grid's cell. Answers the column's width,
    /// the right edge of each number's run in sixteenths, each bar the paint
    /// draws, and the theme's divider color.
    fn numbered_preview(scaled: bool) -> (u16, Vec<i16>, Vec<BarDraw>, [u8; 3]) {
        let mut h = crate::test_harness::TestHarness::with_size(120, 40);
        let root = PathBuf::from("/numbered-preview");
        let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        h.fake_fs().insert_file(root.join("a.txt"), text.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();

        let theme = h.stoat.theme.clone();
        let finder = h.stoat.file_finder.take().expect("the finder is open");
        let snapshot = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(finder.active_core_ref().preview.editor)
            .expect("the preview's editor")
            .display_map
            .snapshot();
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let width = paint_preview_numbers(
            &snapshot, 0, area, &theme, &mut buf, &mut scene, scaled, None,
        );

        let (mut ends, mut bars) = (Vec::new(), Vec::new());
        for command in decode_stream(scene.bytes()) {
            match command {
                Command::TextRun(run) => {
                    let advance = text_run::advance_sixteenths(run.text.len(), run.scale);
                    ends.push(run.col + advance as i16);
                },
                Command::Bar(bar) => bars.push((bar.x, bar.y, bar.width, bar.height, bar.color)),
                _ => {},
            }
        }
        let divider = style_rgb(theme.get(scope::UI_DIVIDER).fg).expect("the theme sets a divider");
        (width, ends, bars, divider)
    }

    /// A grid preview marks the row its target line starts on. Line 3, synced a
    /// third of the way down six rows, scrolls the pane one line, so the mark
    /// sits on row 2. That row's cells take the list's selected background, an
    /// accent bar 4 sixteenths wide stands at its left edge, and its number
    /// draws in the text color. A preview with no target marks nothing.
    #[test]
    fn a_preview_marks_its_target_row() {
        let (theme, marked) = marked_preview(Some(3));
        let (_, unmarked) = marked_preview(None);
        let rgb = |name: &str| style_rgb(theme.get(name).fg).expect("an RGB color");
        let (text, muted) = (rgb(scope::UI_TEXT), rgb(scope::UI_TEXT_MUTED));
        let accent = rgb(scope::UI_BORDER_FOCUSED);

        assert_eq!(
            marked,
            (
                vec![2],
                vec![(0, 32, 4, 16, accent)],
                vec![muted, muted, text, muted, muted, muted],
            ),
        );
        assert_eq!(unmarked, (vec![], vec![], vec![muted; 6]));
    }

    /// Paint a finder's live grid preview of a 10-line file into six rows,
    /// synced to 0-based `target` when it is set. Answers the theme and what
    /// the paint marks.
    fn marked_preview(target: Option<u32>) -> (Arc<Theme>, PreviewMarks) {
        let mut h = crate::test_harness::TestHarness::with_size(120, 40);
        let root = PathBuf::from("/marked-preview");
        let file = root.join("a.txt");
        let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        h.fake_fs().insert_file(file.clone(), text.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();

        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 40, 6);
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let preview = &mut finder.active_core().preview;
        let index = h.stoat.active_workspace;
        let ws = &mut h.stoat.workspaces[index];
        if let Some(line) = target {
            let (fs, languages) = (&*h.stoat.fs_host, &h.stoat.language_registry);
            let source = PreviewSource::File(file);
            preview.sync_at_line(ws, fs, languages, source, line, usize::from(area.height));
        }
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        render_picker_preview(preview, area, &theme, &chrome, ws, &mut buf, &mut scene);

        let selected = theme.list_selected(scope::UI_SELECTION).bg;
        let marked_rows = (area.top()..area.bottom())
            .filter(|&y| (area.left()..area.right()).all(|x| Some(buf[(x, y)].bg) == selected))
            .collect();
        let (mut bars, mut runs) = (Vec::new(), Vec::new());
        for command in decode_stream(scene.bytes()) {
            match command {
                Command::Bar(bar) => bars.push((bar.x, bar.y, bar.width, bar.height, bar.color)),
                Command::TextRun(run) => runs.push((run.row, run.color)),
                _ => {},
            }
        }
        // A run batch groups its runs by color, so the stream is not top down.
        runs.sort_unstable();
        let colors = runs.into_iter().map(|(_, color)| color).collect();
        (theme, (marked_rows, bars, colors))
    }

    /// A path breaks only after a `/`, so each line takes the whole segments
    /// that fit, and a path that fits takes one line.
    #[test]
    fn a_path_breaks_into_lines_at_its_slashes() {
        let lines = |label: &str| {
            let mut starts = Vec::new();
            label_lines(label, true, 55, &mut starts);
            starts
        };

        assert_eq!(
            [
                lines("stoat/src/render/editor/minimap/emit/apc_pool_scene_builder.rs"),
                lines(
                    "vendor/tree-sitter-markdown/tree-sitter-markdown-inline/src/scene_scanner.c"
                ),
                lines("stoat/src/render/editor/minimap/emit/scene.rs"),
            ],
            [vec![0, 37], vec![0, 28], vec![0]],
        );
    }

    /// A segment longer than a whole line starts on a fresh line and breaks at
    /// the last cell of each line, so no char is cut. This path at width 47
    /// breaks at its slashes up to its 51-char file name, which fills a fresh
    /// line of 43 chars and leaves 8 for the next.
    #[test]
    fn a_segment_longer_than_a_line_breaks_at_the_last_cell_of_each_line() {
        let path = "vendor/github.com/tree-sitter-grammars/tree-sitter-markdown/\
                    tree-sitter-markdown-inline/src/generated/scanner/\
                    scene_external_scanner_state_serialization_tables.c";
        let mut starts = Vec::new();
        label_lines(path, true, 47, &mut starts);

        assert_eq!((path.len(), starts), (161, vec![0, 39, 60, 102, 110, 153]));
    }

    /// A relative entry is one segment, so it breaks at the last cell of each
    /// line, 17 chars on the first line and 13 on each later one. A label one
    /// char too long for its line puts that char on a line of its own.
    #[test]
    fn a_label_that_is_no_path_breaks_at_the_last_cell_of_each_line() {
        let lines = |label: &str| {
            let mut starts = Vec::new();
            label_lines(label, false, 17, &mut starts);
            starts
        };

        assert_eq!(
            [
                lines("feature/a-long-branch-name-for-the-cut"),
                lines("feature/a-long-bra"),
            ],
            [vec![0, 17, 30], vec![0, 17]],
        );
    }

    /// The Wrap Mark of a continuation line takes the foreground of
    /// `ui.virtual.wrap`, or that of `ui.text.disabled` in a theme that leaves
    /// the wrap scope unset.
    #[test]
    fn the_wrap_mark_reads_in_the_wrap_scope_or_in_the_disabled_text() {
        let list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 2);
        let mark = |body: &str| {
            let buf = painted_list(&list, area, from_result(0), &theme_of(body));
            (buf[(3, 1)].symbol().to_owned(), buf[(3, 1)].fg)
        };

        assert_eq!(
            [
                mark(r##"ui.virtual.wrap.fg = "#010203";"##),
                mark(r##"ui.text.disabled.fg = "#040506";"##),
            ],
            [
                ("\u{21b3}".to_owned(), Color::Rgb(1, 2, 3)),
                ("\u{21b3}".to_owned(), Color::Rgb(4, 5, 6)),
            ],
        );
    }

    /// A selected result that wraps draws one rounded box over all of its rows
    /// in a live scene.
    #[test]
    fn a_selected_result_boxes_all_of_its_rows() {
        let mut list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![]]);
        list.selected = 0;
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_path_rows(
            &rows_of(&list, None),
            RowMarks::Modified(&[]),
            area,
            from_result(0),
            &rounded_row_theme(),
            Style::default(),
            true,
            None,
            &mut buf,
            &mut scene,
        );

        assert_eq!(
            row_boxes(scene.bytes(), area),
            rounded_row_boxes(ROW_SELECT)
        );
    }

    /// A page that starts two rows into a wrapped result paints that result's
    /// later lines from its top row.
    #[test]
    fn a_page_that_starts_inside_a_result_paints_its_later_lines() {
        let list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 2);
        let start = RowStart { result: 0, skip: 2 };
        let buf = painted_list(&list, area, start, &Theme::empty());

        assert_eq!(
            row_texts(&buf, area),
            ["   \u{21b3} module/", "   \u{21b3} file_name.rs"]
        );
    }

    /// The window ends with the selected result and holds it whole, and it
    /// starts at the first result while the results from the first fit.
    #[test]
    fn the_window_ends_with_the_selected_result_and_holds_it_whole() {
        let ones = |selected| window_from(20, selected, 12, |_| 1).first;
        let mixed = [1, 3, 1, 2, 1, 1];
        let wrapped = window_from(mixed.len(), 3, 4, |result| mixed[result]);
        let tall = [1, 6, 1];
        let taller = window_from(tall.len(), 1, 4, |result| tall[result]);

        assert_eq!((ones(3), ones(14)), (0, 3));
        assert_eq!(
            (
                wrapped.clone(),
                (0..4).map(|row| wrapped.result_at(row)).collect::<Vec<_>>(),
                wrapped.whole(4),
            ),
            (
                ListWindow {
                    first: 2,
                    heights: vec![1, 2, 1],
                },
                vec![Some(2), Some(3), Some(3), Some(4)],
                3,
            ),
        );
        assert_eq!(
            (taller.first, taller.result_at(3), taller.whole(4)),
            (1, Some(1), 0)
        );
    }

    /// The pixel list of the scale-1 cell of 9 by 18 px.
    const PIXELS: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };

    /// The white that a run takes from a theme that colors no text.
    const UNTHEMED: [u8; 3] = [0xff; 3];

    /// The Wrap Mark's color under [`pixel_theme`].
    const WRAP: [u8; 3] = [1, 2, 3];

    /// A theme whose selection and Wrap Mark are RGB and whose text takes no
    /// color, so a pixel list's runs read apart by weight and the mark.
    fn pixel_theme() -> Theme {
        theme_of(r##"ui.selection.bg = "#040506"; ui.virtual.wrap.fg = "#010203";"##)
    }

    /// `a.rs`, [`LONG_PATH`], and `b.rs`, with the long path selected.
    fn wrapped_selection() -> PickList {
        let paths = ["/r/a.rs", LONG_PATH, "/r/b.rs"];
        let mut list = list_of(paths.map(PathBuf::from).to_vec(), vec![vec![]; 3]);
        list.selected = 1;
        list
    }

    /// The commands that a pixel paint of `list` from `start` into `area` sends,
    /// without the ends of its text runs, and whether every cell of the area
    /// stays blank.
    fn pixel_paint(list: &PickList, area: Rect, start: RowStart) -> (Vec<Command>, bool) {
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_path_rows(
            &rows_of(list, None),
            RowMarks::Modified(&[]),
            area,
            start,
            &pixel_theme(),
            Style::default(),
            false,
            Some(PIXELS),
            &mut buf,
            &mut scene,
        );
        let commands = decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect();
        (commands, buf == Buffer::empty(area))
    }

    fn pixel_run(col: i16, row: i16, text: &str, color: [u8; 3], bold: bool) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row,
            scale: FULL_CELL_SCALE,
            color,
            bg: None,
            bold,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    /// Each result is a 20 px box 1 px from the next. `a.rs` draws 1 px down
    /// from the list's top, the selected long path's box stands at pixel row
    /// 21, 3 px into cell row 1, and its four lines stand 1 px inside it, from
    /// 4 px into cell row 1. `b.rs` follows the 75 px of that box and gap, 7 px
    /// into cell row 5.
    #[test]
    fn a_pixel_list_draws_each_line_as_runs_under_its_nudge() {
        let area = Rect::new(0, 0, 20, 10);
        let box_bar = Command::Bar(BarCommand {
            x: 2,
            y: 16,
            width: 313,
            height: 66,
            color: ROW_SELECT,
            occludes: false,
            hatch: None,
            radius: 4,
            corners: 0,
        });

        assert_eq!(
            pixel_paint(&wrapped_selection(), area, RowStart { result: 0, skip: 0 }),
            (
                vec![
                    Command::Nudge([0, 1]),
                    pixel_run(16, 0, "a.rs", UNTHEMED, true),
                    Command::Nudge([0, 3]),
                    box_bar,
                    Command::Nudge([0, 4]),
                    pixel_run(16, 16, "very/deeply/", UNTHEMED, false),
                    pixel_run(48, 32, "\u{21b3}", WRAP, false),
                    pixel_run(80, 32, "nested/dir/", UNTHEMED, false),
                    pixel_run(48, 48, "\u{21b3}", WRAP, false),
                    pixel_run(80, 48, "module/", UNTHEMED, false),
                    pixel_run(48, 64, "\u{21b3}", WRAP, false),
                    pixel_run(80, 64, "file_name.rs", UNTHEMED, true),
                    Command::Nudge([0, 7]),
                    pixel_run(16, 80, "b.rs", UNTHEMED, true),
                    Command::Nudge([0, 0]),
                ],
                true,
            )
        );
    }

    /// A page that starts 30 px into the long path's box shows its last two
    /// lines from pixel row 7, and `b.rs` from pixel row 46. The box's top lies
    /// on the page above, so this page draws no box.
    #[test]
    fn a_pixel_page_draws_only_the_lines_that_start_in_it() {
        let area = Rect::new(0, 0, 20, 3);

        assert_eq!(
            pixel_paint(
                &wrapped_selection(),
                area,
                RowStart {
                    result: 1,
                    skip: 30
                }
            )
            .0,
            [
                Command::Nudge([0, 7]),
                pixel_run(48, 0, "\u{21b3}", WRAP, false),
                pixel_run(80, 0, "module/", UNTHEMED, false),
                pixel_run(48, 16, "\u{21b3}", WRAP, false),
                pixel_run(80, 16, "file_name.rs", UNTHEMED, true),
                Command::Nudge([0, 10]),
                pixel_run(16, 32, "b.rs", UNTHEMED, true),
                Command::Nudge([0, 0]),
            ]
        );
    }

    /// A modified file's `M` takes `ui.list.modified` over `diff.modified`, and
    /// a theme that leaves the list scope unset paints it in `diff.modified`.
    #[test]
    fn a_modified_mark_takes_the_list_scope_and_falls_back_to_the_diff_color() {
        let list = list_of(vec![PathBuf::from("/r/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 1);
        let mark = |body: &str| {
            let mut buf = Buffer::empty(area);
            paint_path_rows(
                &rows_of(&list, None),
                RowMarks::Modified(&list.base),
                area,
                RowStart { result: 0, skip: 0 },
                &theme_of(body),
                Style::default(),
                false,
                None,
                &mut buf,
                &mut stoat_widgets::ApcScene::new(),
            );
            (buf[(19, 0)].symbol().to_owned(), buf[(19, 0)].fg)
        };

        assert_eq!(
            [
                mark(r##"ui.list.modified.fg = "#010203"; diff.modified.fg = "#040506";"##),
                mark(r##"diff.modified.fg = "#040506";"##),
            ],
            [
                ("M".to_owned(), Color::Rgb(1, 2, 3)),
                ("M".to_owned(), Color::Rgb(4, 5, 6)),
            ]
        );
    }

    /// The commands of a pixel paint of `a.rs` and `b.rs` under `theme` in a
    /// list 20 cells wide and 2 rows high, where `a.rs` takes the mark of
    /// `marks`, without the ends of the text runs.
    fn marked_pixel_paint(marks: fn(&[PathBuf]) -> RowMarks<'_>, theme: &Theme) -> Vec<Command> {
        let paths = ["/r/a.rs", "/r/b.rs"].map(PathBuf::from);
        let list = list_of(paths.to_vec(), vec![vec![]; 2]);
        let area = Rect::new(0, 0, 20, 2);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_path_rows(
            &rows_of(&list, None),
            marks(&paths[..1]),
            area,
            RowStart { result: 0, skip: 0 },
            theme,
            Style::default(),
            false,
            Some(PIXELS),
            &mut Buffer::empty(area),
            &mut scene,
        );
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// A modified path ends its first line in an `M` at the Caption size, a run
    /// of its own that ends 11 px, 20 sixteenths, inside the list's right edge
    /// at col 320. The cells of the label hold no `M`.
    #[test]
    fn a_pixel_row_draws_its_mark_at_the_caption_size() {
        let theme = theme_of(r##"diff.modified.fg = "#0a0b0c";"##);

        assert_eq!(
            marked_pixel_paint(|paths| RowMarks::Modified(paths), &theme),
            [
                Command::Nudge([0, 1]),
                pixel_run(16, 0, "a.rs", UNTHEMED, true),
                pixel_run(284, 0, "M", [10, 11, 12], false),
                Command::Nudge([0, 4]),
                pixel_run(16, 16, "b.rs", UNTHEMED, true),
                Command::Nudge([0, 0]),
            ]
        );
    }

    /// A modified path that wraps draws its `M` on its first line alone.
    #[test]
    fn a_wrapped_pixel_row_draws_its_mark_on_its_first_line_alone() {
        let list = list_of(vec![PathBuf::from(LONG_PATH)], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 4);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_path_rows(
            &rows_of(&list, None),
            RowMarks::Modified(&list.base),
            area,
            RowStart { result: 0, skip: 0 },
            &theme_of(r##"diff.modified.fg = "#0a0b0c";"##),
            Style::default(),
            false,
            Some(PIXELS),
            &mut Buffer::empty(area),
            &mut scene,
        );
        let marks: Vec<(i16, i16)> = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.text == "M" => Some((run.col, run.row)),
                _ => None,
            })
            .collect();

        assert_eq!(marks, [(284, 0)]);
    }

    /// The dot of an unsaved buffer whose color is no RGB value draws as the
    /// `●` glyph where the `M` would stand, in the color a run falls back to.
    #[test]
    fn a_pixel_dot_with_no_rgb_color_draws_its_glyph() {
        let theme = theme_of("ui.list.dirty.fg = red;");

        assert_eq!(
            marked_pixel_paint(|paths| RowMarks::Dirty(paths), &theme),
            [
                Command::Nudge([0, 1]),
                pixel_run(16, 0, "a.rs", UNTHEMED, true),
                pixel_run(284, 0, "\u{25cf}", UNTHEMED, false),
                Command::Nudge([0, 4]),
                pixel_run(16, 16, "b.rs", UNTHEMED, true),
                Command::Nudge([0, 0]),
            ]
        );
    }

    /// An unsaved buffer ends its first line in a dot 6 px across that ends
    /// 11 px inside the list's right edge, 8 px inside the box of its row, and
    /// stands 6 px under the top of that box.
    #[test]
    fn a_pixel_row_draws_its_dot_eight_pixels_inside_its_box() {
        let theme = theme_of(r##"ui.list.dirty.fg = "#0d0e0f";"##);
        let dot = Command::Bar(BarCommand {
            x: 289,
            y: 0,
            width: 11,
            height: 5,
            color: [13, 14, 15],
            occludes: false,
            hatch: None,
            radius: 3,
            corners: 0,
        });

        assert_eq!(
            marked_pixel_paint(|paths| RowMarks::Dirty(paths), &theme),
            [
                Command::Nudge([0, 1]),
                pixel_run(16, 0, "a.rs", UNTHEMED, true),
                Command::Nudge([0, 6]),
                dot,
                Command::Nudge([0, 4]),
                pixel_run(16, 16, "b.rs", UNTHEMED, true),
                Command::Nudge([0, 0]),
            ]
        );
    }

    /// In a page 36 px high, `c.rs` 8 px into the list draws from pixel row 35
    /// on the page's last cell row, and 7 px into the list its pixel row 36
    /// belongs to the next page.
    #[test]
    fn a_line_on_the_last_pixel_row_of_a_page_stays_on_that_page() {
        let paths = ["/r/a.rs", "/r/b.rs", "/r/c.rs"];
        let list = list_of(paths.map(PathBuf::from).to_vec(), vec![vec![]; 3]);
        let area = Rect::new(0, 0, 20, 2);
        let paint = |skip| pixel_paint(&list, area, RowStart { result: 0, skip }).0;

        assert_eq!(
            [paint(8), paint(7)],
            [
                vec![
                    Command::Nudge([0, 14]),
                    pixel_run(16, 0, "b.rs", UNTHEMED, true),
                    Command::Nudge([0, 17]),
                    pixel_run(16, 16, "c.rs", UNTHEMED, true),
                    Command::Nudge([0, 0]),
                ],
                vec![
                    Command::Nudge([0, 15]),
                    pixel_run(16, 0, "b.rs", UNTHEMED, true),
                    Command::Nudge([0, 0]),
                ],
            ]
        );
    }

    /// The row tops sum the heights of the results before each one, and a
    /// page starts on the result its first row falls in. A refilter or another
    /// list width leaves the table unanswered until a pool measures again.
    #[test]
    fn the_row_tops_sum_the_heights_before_each_result() {
        let base = [
            Path::new("/r/a.rs"),
            Path::new(LONG_PATH),
            Path::new("/r/b.rs"),
        ];
        let mut list = list_of(base.map(PathBuf::from).to_vec(), vec![vec![]; 3]);
        ensure_row_tops(&mut list, Path::new("/r"), None, "", 20, 2);
        let tops = row_tops(&list, 20, "").map(<[u32]>::to_vec);
        let starts = tops
            .as_deref()
            .map(|tops| (page_start(tops, 3), page_start(tops, 6)));
        let other_width = row_tops(&list, 21, "").is_some();
        list.filter_generation += 1;
        let refiltered = row_tops(&list, 20, "").is_some();

        assert_eq!(
            (tops, starts, other_width, refiltered),
            (
                Some(vec![0, 1, 5, 6]),
                Some((Some(RowStart { result: 1, skip: 2 }), None)),
                false,
                false,
            ),
        );
    }
}
