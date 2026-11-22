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
    picker::{write_row_display, PickList, Preview, ScaledPreview},
    render::{
        chrome::{PlateColors, TypeTag},
        editor::{self, render_editor, ResolvedChrome},
        paint::style_rgb,
        text::{clip_to_width, write_str, write_str_clipped},
    },
    theme::{scope, Theme, TypeRole},
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
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_widgets::{
    bar::Bar,
    panel::Panel,
    pool,
    text_run::{self, TextRun, TextRunBatch},
};
use stoatty_protocol::command::Edge;
use unicode_width::UnicodeWidthStr;

/// The glyph before a filter modal's query. The command palette keeps `:` in
/// its place, the command-line prompt that its commands follow.
pub(crate) const FILTER_PROMPT: &str = "\u{203A}";

/// Columns the query keeps on a filter modal's header row. A row too narrow
/// for the query and the words right of it drops the chip first, then the
/// count.
const MIN_QUERY_COLUMNS: u16 = 12;

/// The logical px a plate's header row draws right and down of its cells.
///
/// The plate's side stands 11 px outside the row, so 1 px right sets the row's
/// text 12 px in from the side, as the boards pad the header. 7 px down centers
/// an 18 px row in the boards' 32 px band.
const PLATE_HEADER_NUDGE: [i16; 2] = [1, 7];

/// Sixteenths between two parts of a plate's header row, the boards' 8 px gap
/// at a 9 px cell.
const PLATE_HEADER_GAP: u16 = 14;

/// Sixteenths of fill on each side of an active chip's text, the boards' 6 px
/// padding at a 9 px cell.
const CHIP_PAD: u16 = 11;

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
    /// The glyph before the query, [`FILTER_PROMPT`] in all but the palette.
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
/// `caret`, the char its cursor covers. The caret's char draws in the ground
/// over the text color, which is the block caret. A label or a prompt that
/// runs past the row's end clips there. A count of no shown entries paints its
/// zero in `diff.deleted`'s foreground, as [`paint_header_row`] does. A part
/// whose color has no RGB value draws nothing.
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
        plate_chip_boxes(&trailing, fill, inner, scene);
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
    if let (Some(at_caret), Some(caret_color)) = (at_caret, text) {
        let caret_x = query_x + text_run::advance_sixteenths(before.chars().count(), ui);
        run(caret_x, at_caret, ui, (Some(ground), caret_color), false);
        let after_x = query_x + text_run::advance_sixteenths(before.chars().count() + 1, ui);
        run(after_x, after, ui, (text, ground), false);
    }

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

    scene.nudge([0, 0]);
}

/// Blank `row`'s cells to the plate's `ground`, under the runs of a plate's
/// header or key line.
fn blank_plate_row(buf: &mut Buffer, row: Rect, ground: [u8; 3]) {
    let [r, g, b] = ground;
    for x in row.left()..row.right() {
        buf[(x, row.y)]
            .set_char(' ')
            .set_style(Style::default().bg(Color::Rgb(r, g, b)));
    }
}

/// Draw a chip panel in `fill` over the cells of each boxed chip that
/// [`plate_trailing`] placed on `inner`'s first row.
///
/// The panels must follow the row's [`PLATE_HEADER_NUDGE`], so they stand off
/// the cells as the chips' runs do.
fn plate_chip_boxes(
    trailing: &PlateTrailing,
    fill: [u8; 3],
    inner: Rect,
    scene: &mut stoat_widgets::ApcScene,
) {
    for (_, cells) in &trailing.chips {
        if let Some(cells) = cells {
            Panel {
                edges: Edge::symmetric(0, 0),
                ..Panel::chip(fill)
            }
            .draw_components(
                Rect::new(inner.x + cells.start, inner.y, cells.len() as u16, 1),
                scene,
            );
        }
    }
}

/// The places of a plate header's count and chips, which end at its right.
#[derive(Debug, PartialEq)]
struct PlateTrailing {
    /// The count's run start, in sixteenths from the row's left.
    count: Option<u16>,
    /// Each kept chip's run start, and the cells its box covers.
    chips: Vec<(u16, Option<Range<u16>>)>,
    /// The sixteenth the query ends by.
    query_end: u16,
}

/// Place a plate header's `count` and `chips` right to left from `end`
/// sixteenths, [`PLATE_HEADER_GAP`] apart, at `scale`.
///
/// With `boxed`, an active chip takes whole cells, because its box is a panel.
/// The box is the fewest cells that hold the chip's text with [`CHIP_PAD`] on
/// each side, and it ends at the last cell edge at or left of the parts after
/// it. The chip's run centers in the box. Every other part ends flush where the
/// parts after it leave off.
///
/// If the query then ends left of `query_min`, the chips drop together. If it
/// still does, the count drops too, as [`paint_header_row`] drops them.
///
/// A [`PlateHeader`]'s meta takes the count's place, and its title the query's.
fn plate_trailing(
    count: Option<&str>,
    chips: &[(&str, bool)],
    boxed: bool,
    end: u16,
    query_min: u16,
    scale: u16,
) -> PlateTrailing {
    let advance = |text: &str| text_run::advance_sixteenths(text.chars().count(), scale);
    let place = |count: Option<&str>, chips: &[(&str, bool)]| {
        let mut right = end;
        let mut placed = Vec::with_capacity(chips.len());
        for &(chip, active) in chips.iter().rev() {
            let width = advance(chip);
            if active && boxed {
                let last = right / 16;
                let first = last.saturating_sub((width + 2 * CHIP_PAD).div_ceil(16));
                let col = first * 16 + ((last - first) * 16).saturating_sub(width) / 2;
                placed.push((col, Some(first..last)));
                right = first * 16;
            } else {
                right = right.saturating_sub(width);
                placed.push((right, None));
            }
            right = right.saturating_sub(PLATE_HEADER_GAP);
        }
        placed.reverse();

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
        plate_chip_boxes(&trailing, fill, inner, scene);
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
    /// A note that ends the line, such as the context a modal opened in.
    pub(crate) note: Option<&'a str>,
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
/// `ui.text.muted`. The legend's spans keep their own styles, and the note
/// reads in `ui.text.muted`. The legend is always kept, and the first hint
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
    if let (Some(note), Some(at)) = (footer.note, places.note) {
        write_str_clipped(buf, x + at, y, note, word_style, row_end);
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
    if let (Some(note), Some(at)) = (footer.note, places.note) {
        run(at, note, muted);
    }
    scene.nudge([0, 0]);
}

/// A run of `text` that starts `col` sixteenths into a plate's header or key
/// line row, in `color` over an opaque `bg`, at `scale`.
fn plate_run(
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

    let note = footer
        .note
        .filter(|&note| width(note) <= end)
        .map(|note| end - width(note));
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
pub(crate) fn target_picker_layout(
    area: Rect,
    zoom: i8,
    list_percent: u16,
) -> Option<PickerLayout> {
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
        list_percent,
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

/// Paint a picker's result rows into `area`, one repo-relative path per row,
/// with the selected row and fuzzy-match characters highlighted.
///
/// `start_row` is the first filtered index to show, so the live list can derive
/// it from the selection while the smooth-scroll pool paints absolute pages, and
/// both render identical rows. Rows are read from `picklist.base`, which every
/// caller keeps in sync with its display set on refilter.
///
/// `row_style` paints every row but the selected one. A caller passes its own
/// style to set a whole list apart, such as a list of ignored files.
///
/// The selected row draws through [`chrome::selected_row`]. With `panels` it
/// is a rounded box in a live scene. A pool page passes `panels` false,
/// because a page captures no panel, and keeps the row's cells in
/// [`Theme::list_selected`] with the accent bar.
///
/// A path row keeps columns 1 and 2 for its [`chrome::type_tag`], so tagged and
/// untagged rows align, and a wider tag pushes only its own row's label right.
/// Its directory paints in `ui.text.muted` and its name in the row's style. A
/// path found in `marks`, which is sorted, ends in an `M` in the
/// `diff.modified` color. A relative entry, such as a branch name, gets none
/// of these.
///
/// [`chrome::selected_row`]: crate::render::chrome::selected_row
/// [`chrome::type_tag`]: crate::render::chrome::type_tag
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_path_rows(
    picklist: &PickList,
    git_root: &Path,
    home: Option<&Path>,
    prefix: &str,
    marks: &[PathBuf],
    area: Rect,
    start_row: usize,
    theme: &Theme,
    row_style: Style,
    panels: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let match_style = theme.list_match();
    let dir_style = theme.get(scope::UI_TEXT_MUTED);
    let mark_style = theme.get(scope::DIFF_MODIFIED);

    let area_end = area.x + area.width;
    let prefix_len = prefix.chars().count() as u32;
    let mut label = String::new();

    // Rows past the eagerly indexed block derive their offsets here, one row at
    // a time into buffers the whole window shares.
    let mut derived = Vec::new();
    let mut matching = fuzzy::Scratch::default();

    for (row_idx, &idx) in picklist
        .filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let indices = picklist.row_indices(start_row + row_idx, &mut derived, &mut matching);
        let row = area.y + row_idx as u16;
        let style = match start_row + row_idx == picklist.selected {
            true => crate::render::chrome::selected_row(
                Rect::new(area.x, row, area.width, 1),
                row_style,
                panels,
                theme,
                buf,
                scene,
            ),
            false => {
                for col in area.x..area_end {
                    buf[(col, row)].set_char(' ').set_style(row_style);
                }
                row_style
            },
        };

        let path = &picklist.base[idx];
        let is_path = path.is_absolute();
        let label_x = match is_path {
            true => paint_type_tag(path, area.x + 1, row, style, theme, buf, area_end),
            false => area.x + 1,
        };
        let marked = is_path && area_end >= label_x + 3 && marks.binary_search(path).is_ok();
        let end_x = match marked {
            true => {
                buf[(area_end - 1, row)].set_char('M').set_style(mark_style);
                area_end - 2
            },
            false => area_end,
        };

        label.clear();
        label.push_str(prefix);
        write_row_display(
            &mut label,
            path,
            git_root,
            picklist.display_roots.as_deref(),
            home,
        );
        let name_col = if is_path { name_start(&label) } else { 0 };
        let width = end_x.saturating_sub(label_x) as usize;
        let label_len = label.chars().count();

        // A row wider than its column gives up chars to an ellipsis in one cell,
        // leaving `width - 1` cells for the rest. A path is start-truncated
        // helix-style so the file name at the tail stays on screen. A relative
        // entry is a label rather than a path, such as a branch name of the
        // palette's argument list, and its head names it, so its tail goes
        // instead.
        let (dropped, text_x, text_end) = if label_len > width && width > 1 {
            if is_path {
                let dropped = label_len - (width - 1);
                buf[(label_x, row)].set_char('\u{2026}').set_style(style);
                let tail_start = label
                    .char_indices()
                    .nth(dropped)
                    .map_or(label.len(), |(byte, _)| byte);
                write_str_clipped(buf, label_x + 1, row, &label[tail_start..], style, end_x);
                (dropped, label_x + 1, end_x)
            } else {
                let ellipsis_x = end_x - 1;
                write_str_clipped(buf, label_x, row, &label, style, ellipsis_x);
                buf[(ellipsis_x, row)].set_char('\u{2026}').set_style(style);
                (0, label_x, ellipsis_x)
            }
        } else {
            write_str_clipped(buf, label_x, row, &label, style, end_x);
            (0, label_x, end_x)
        };

        for (label_col, _) in label.chars().enumerate().skip(dropped) {
            let col = text_x + (label_col - dropped) as u16;
            if col >= text_end {
                break;
            }
            if label_col < name_col {
                buf[(col, row)].set_style(dir_style);
            }
            let label_col = label_col as u32;
            // The literal `prefix` is never matched. The picklist's indices are
            // offsets into the part after it, so shift them past the prefix.
            if label_col >= prefix_len && indices.binary_search(&(label_col - prefix_len)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

/// Columns a path row keeps for its type tag, which fit a two-letter tag such
/// as `rs`.
const TAG_COLUMNS: u16 = 2;

/// Paint `path`'s type tag from column `x` of `row`, and return the column its
/// label starts at.
///
/// The label starts one blank column past [`TAG_COLUMNS`], or past a tag wider
/// than them.
fn paint_type_tag(
    path: &Path,
    x: u16,
    row: u16,
    style: Style,
    theme: &Theme,
    buf: &mut Buffer,
    end_x: u16,
) -> u16 {
    let width = match crate::render::chrome::type_tag(path) {
        Some(TypeTag {
            tag,
            scope: tag_scope,
            ..
        }) => {
            write_str_clipped(buf, x, row, tag, style.patch(theme.get(tag_scope)), end_x);
            tag.width() as u16
        },
        None => 0,
    };
    x + width.max(TAG_COLUMNS) + 1
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
/// the same numbers.
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
    let Some(editor) = ws.editors.get_mut(preview.editor) else {
        return area.height;
    };

    let grid = preview_grid(area, scene.scaled_cell()).filter(|_| scene.live());
    let Some((rows, cols)) = grid else {
        paint_preview_editor(editor, numbers, area, theme, chrome, buf, scene);
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
        numbers,
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
/// its editor out in, or `None` without a scaled cell, where it keeps the grid.
fn preview_grid(area: Rect, cell: Option<[u16; 2]>) -> Option<(u16, u16)> {
    cell.map(|cell| pool::scaled_grid(area.width, area.height, cell, PREVIEW_SCALE))
}

/// Paint `editor` into `area` of `buf` as a preview, and record in its
/// `gutter_width` the number column `numbers` leads its lines with.
fn paint_preview_editor(
    editor: &mut EditorState,
    numbers: bool,
    area: Rect,
    theme: &Theme,
    chrome: &ResolvedChrome,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let fallback = theme.get(scope::UI_TEXT);

    // The paint writes `gutter_width` on most paths, but a scroll past the end
    // returns before the write, so the column is set here on both branches.
    if !numbers {
        render_editor(editor, area, fallback, theme, Some(chrome), buf, false);
        editor.gutter_width = 0;
        return;
    }

    let columns = preview_number_columns(editor.display_map.buffer_snapshot().line_count());
    let text = Rect {
        x: area.x + columns,
        width: area.width.saturating_sub(columns),
        ..area
    };
    render_editor(editor, text, fallback, theme, Some(chrome), buf, false);
    let snapshot = editor.display_map.snapshot();
    paint_preview_numbers(&snapshot, editor.scroll_row, area, theme, buf, scene);
    editor.gutter_width = columns;
}

/// Sixteenths between a preview number's right edge and its line's text, the
/// board's 14 px at the 9 px cell.
const PREVIEW_NUMBER_GAP: u16 = 24;

/// The fewest digits a preview's number column holds.
///
/// Four digits and the cell before the text are the board's 44 px column, so
/// a short file's text starts where a long file's does.
const PREVIEW_NUMBER_DIGITS: u16 = 4;

/// Cells the number column of a preview over `line_count` lines takes.
///
/// The column holds the widest number, at least [`PREVIEW_NUMBER_DIGITS`]
/// wide, and one cell before the text.
pub(crate) fn preview_number_columns(line_count: u32) -> u16 {
    editor::gutter_digits(line_count).max(PREVIEW_NUMBER_DIGITS) + 1
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
/// The live preview and the pages that glide it both paint their numbers
/// through this, so the numbers hold still across the handoff between the two.
pub(crate) fn paint_preview_numbers(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    let width = preview_number_columns(snapshot.buffer_line_count());
    let visible = snapshot
        .line_count()
        .saturating_sub(top_row)
        .min(u32::from(area.height));
    let (lines, _) = editor::gutter_geometry(snapshot, top_row, visible);

    let muted = theme.get(scope::UI_TEXT_MUTED);
    let color = style_rgb(muted.fg).filter(|_| scene.live());
    let scale = theme.type_scale(TypeRole::Caption);
    let run_end = width * 16 - PREVIEW_NUMBER_GAP;
    let digits_end = area.x + width - 1;

    let mut runs = TextRunBatch::default();
    let mut number = String::new();
    let mut y = area.y;
    for (line, height) in lines {
        number.clear();
        write!(number, "{line}").expect("writing to a String is infallible");

        match color {
            Some(color) => {
                let advance = text_run::advance_sixteenths(number.len(), scale);
                runs.push(
                    Rect::new(area.x, y, 1, 1),
                    TextRun {
                        col: run_end.saturating_sub(advance) as i16,
                        row: 0,
                        scale,
                        color,
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
                write_str_clipped(buf, x, y, &number, muted, end_x);
            },
        }
        y += height;
    }
    runs.flush(scene);

    width
}

/// Share of a modal's body its list pane takes, before the user drags the
/// separator to a share of their own.
///
/// One value for the whole modal family, so an untouched finder, symbol list,
/// code search, and commit picker all divide their body the same way.
pub(crate) const DEFAULT_LIST_PERCENT: u16 = 40;

/// Columns each pane of a side-by-side modal keeps.
///
/// A list narrower than this cannot show a path worth reading, and a preview
/// narrower cannot show code, so the split refuses to squeeze either below it
/// however the share is set or dragged.
pub(crate) const MIN_PANE_COLUMNS: u16 = 24;

/// Split a picker body rect into a result list and an optional preview pane.
///
/// The preview appears only when `width >= wide_threshold`, where the list
/// takes `list_percent` of the width (floored at `min_list`) and the preview the
/// rest past a one-cell separator. Below the threshold the list takes the full
/// width and there is no preview.
pub(crate) fn split_list_preview(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    wide_threshold: u16,
    min_list: u16,
    list_percent: u16,
) -> (Rect, Option<Rect>) {
    if width >= wide_threshold {
        let list_width = (width * list_percent / 100).max(min_list);
        let preview_width = width.saturating_sub(list_width + 1);
        (
            Rect::new(x, y, list_width, height),
            Some(Rect::new(x + list_width + 1, y, preview_width, height)),
        )
    } else {
        (Rect::new(x, y, width, height), None)
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::match_theme, *};
    use crate::{input_view::SubmitTarget, picker::INDEXED_ROWS, test_fixture};
    use ratatui::{buffer::Cell, style::Color};
    use std::path::PathBuf;
    use stoatty_protocol::command::{
        decode_stream, encode_bar, BarCommand, BorderStyle, Command, Edge, PanelCommand,
        PanelShadow, TextRunCommand,
    };

    /// The header a finder-like modal paints, its chip at the row's end.
    const FINDER_HEADER: FilterHeader<'static> = FilterHeader {
        label: "Files",
        prompt: FILTER_PROMPT,
        count: Some((14, 890)),
        chips: &[("S-Tab \u{00B7} git only", true)],
    };

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

    /// The buffer and the scene of `header` painted into the inner rect
    /// (2, 1, 60, 4) over an input that holds `ab`, under a theme of the
    /// `PLATE_` colors, by a scene that reads nudges where `nudges` holds.
    ///
    /// Every cell of the 64 by 6 buffer holds `x` before the paint.
    fn painted_header(
        header: &FilterHeader<'_>,
        nudges: bool,
    ) -> (Buffer, stoat_widgets::ApcScene) {
        let theme = {
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
        };
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
    /// 14 sixteenths apart. The caret is a space after `ab`, in the ground over
    /// the text color. From the right, `all` ends the row flush, and the active
    /// `applicable` centers in a 12-cell chip panel that ends on the cell edge
    /// 14 sixteenths left of `all`. The count ends 14 sixteenths left of that
    /// box.
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
                Command::Panel(PanelCommand {
                    top: 1,
                    left: 46,
                    width: 12,
                    height: 1,
                    style: BorderStyle::None_,
                    border: PLATE_CHIP_FILL,
                    corner_radius: 4,
                    fill: Some(PLATE_CHIP_FILL),
                    shadow: PanelShadow::None_,
                    edges: Edge::symmetric(0, 0),
                    corners: 0,
                    fillets: 0,
                    fillet_radius: 2,
                    hatch: None,
                    above_pools: false,
                    backdrop: false,
                    anchor: None,
                }),
                plate_run(32, "Help", PLATE_LABEL, PLATE_GROUND, true),
                plate_run(110, FILTER_PROMPT, PLATE_MUTED, PLATE_GROUND, false),
                plate_run(140, "ab", PLATE_TEXT, PLATE_GROUND, false),
                plate_run(172, " ", PLATE_GROUND, PLATE_TEXT, false),
                plate_run(578, "14 of 890", PLATE_MUTED, PLATE_GROUND, false),
                plate_run(752, "applicable", PLATE_CHIP_TEXT, PLATE_CHIP_FILL, false),
                plate_run(944, "all", PLATE_MUTED, PLATE_GROUND, false),
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
                plate_run(94, " ", PLATE_GROUND, PLATE_TEXT, false),
                plate_run(864, "0", PLATE_DELETED, PLATE_GROUND, false),
                plate_run(880, " of 214", PLATE_MUTED, PLATE_GROUND, false),
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
            [324, 578, 322, 464],
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
    /// sixteenths after it. From the right, the chip centers in a 21-cell chip
    /// panel that ends on the row's last cell edge. The meta's spans end 14
    /// sixteenths left of that box, each in its own color.
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
                    Command::Panel(PanelCommand {
                        top: 1,
                        left: 41,
                        width: 21,
                        height: 1,
                        style: BorderStyle::None_,
                        border: PLATE_CHIP_FILL,
                        corner_radius: 4,
                        fill: Some(PLATE_CHIP_FILL),
                        shadow: PanelShadow::None_,
                        edges: Edge::symmetric(0, 0),
                        corners: 0,
                        fillets: 0,
                        fillet_radius: 2,
                        hatch: None,
                        above_pools: false,
                        backdrop: false,
                        anchor: None,
                    }),
                    plate_run(32, "Commit", PLATE_LABEL, PLATE_GROUND, true),
                    plate_run(142, "two", PLATE_TEXT, PLATE_GROUND, true),
                    plate_run(498, "fake \u{b7} ", PLATE_MUTED, PLATE_GROUND, false),
                    plate_run(610, "c1", PLATE_SHA, PLATE_GROUND, false),
                    plate_run(
                        672,
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
    /// `note`, painted `width` columns wide.
    fn cell_footer_line(width: u16, note: Option<&str>) -> String {
        let footer = Footer {
            hints: &FOOTER_HINTS,
            note,
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
                cell_footer_line(40, Some("normal")),
                cell_footer_line(30, Some("normal")),
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
        let footer = Footer {
            hints: &[("ab", "cd")],
            legend: &legend,
            note: Some("nn"),
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
        let footer = Footer {
            hints: &FOOTER_HINTS[..1],
            legend: &legend,
            note: Some("nn"),
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
        let split = |percent| split_list_preview(0, 0, 100, 10, 80, 24, percent);

        assert_eq!(
            split(DEFAULT_LIST_PERCENT),
            (Rect::new(0, 0, 40, 10), Some(Rect::new(41, 0, 59, 10))),
            "the default share splits the body where it always has"
        );
        assert_eq!(
            split(60),
            (Rect::new(0, 0, 60, 10), Some(Rect::new(61, 0, 39, 10))),
            "a wider share moves the boundary and the preview takes the rest"
        );
        assert_eq!(
            split(5),
            (Rect::new(0, 0, 24, 10), Some(Rect::new(25, 0, 75, 10))),
            "a share under the minimum still leaves the list its floor"
        );
    }

    /// A body too narrow for two panes has no separator to drag, so the share is
    /// moot and the list keeps the full width whatever it says.
    #[test]
    fn a_narrow_body_ignores_the_share_and_stays_list_only() {
        assert_eq!(
            split_list_preview(0, 0, 79, 10, 80, 24, 60),
            (Rect::new(0, 0, 79, 10), None),
            "below the wide threshold the list takes everything"
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

    #[test]
    fn long_path_start_truncates_keeping_the_file_name() {
        let git_root = Path::new("/r");
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![]],
        );
        // Column width 20 leaves a 16-cell label area after the pad and the tag
        // slot.
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        assert_eq!(
            row_text(&buf, 0, area),
            " rs \u{2026}le/file_name.rs",
            "an ellipsis stands in for the dropped head, and the file name at the tail stays",
        );
    }

    /// A relative entry is a label rather than a path, such as a branch name of
    /// the palette's argument list, and its head names it. So a cut keeps the
    /// head, and a match in the cut tail paints nothing over the ellipsis.
    #[test]
    fn a_long_label_keeps_its_head() {
        let label = "feature/a-long-branch-name-for-the-cut";
        let list = list_of(vec![PathBuf::from(label)], vec![vec![0, 18]]);
        // Column width 20 leaves a 19-cell label area after the one-cell pad.
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            Path::new("/r"),
            None,
            "",
            &[],
            area,
            0,
            &match_theme(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            (row_text(&buf, 0, area), highlighted),
            (format!(" {}\u{2026}", &label[..18]), vec![1]),
        );
    }

    /// A row outside the repo abbreviates against the home the caller supplies,
    /// which is what keeps the row paint from resolving one itself.
    #[test]
    fn a_row_outside_the_repo_abbreviates_against_the_given_home() {
        let git_root = Path::new("/r");
        let list = list_of(vec![PathBuf::from("/fixture-home/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 24, 1);

        let paint = |home: Option<&Path>| {
            let mut buf = Buffer::empty(area);
            paint_path_rows(
                &list,
                git_root,
                home,
                "",
                &[],
                area,
                0,
                &Theme::empty(),
                Style::default(),
                false,
                &mut buf,
                &mut stoat_widgets::ApcScene::new(),
            );
            row_text(&buf, 0, area).trim().to_string()
        };

        assert_eq!(paint(Some(Path::new("/fixture-home"))), "rs ~/a.rs");
        assert_eq!(
            paint(None),
            "rs /fixture-home/a.rs",
            "and stays whole with no home to measure against"
        );
    }

    #[test]
    fn short_path_renders_in_full_without_an_ellipsis() {
        let git_root = Path::new("/r");
        let list = list_of(vec![PathBuf::from("/r/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let text = row_text(&buf, 0, area);
        assert!(
            !text.contains('\u{2026}'),
            "no ellipsis for a path that fits: {text:?}"
        );
        assert_eq!(text.trim(), "rs a.rs", "the whole path renders: {text:?}");
    }

    #[test]
    fn match_highlight_follows_the_truncated_tail() {
        let git_root = Path::new("/r");
        // The 42-char label loses its leading 27 chars in the 16-cell area past
        // the tag slot, so the tail starts at char 27. Match offset 0 lands in
        // the dropped head and offset 30 (the `f` of `file_name`) in the
        // visible tail.
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![0, 30]],
        );
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &match_theme(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![8],
            "only the tail match highlights at its shifted column; the dropped-head match paints nothing"
        );
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
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            deep,
            &match_theme(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

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
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            550,
            &match_theme(),
            Style::default(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![4, 5, 6, 7],
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
            &list,
            Path::new("/r"),
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default().fg(muted),
            false,
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
}
