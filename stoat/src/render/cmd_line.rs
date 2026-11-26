//! The Cmd Line, the header that leads a prompt where the user types a
//! command.
//!
//! Its first row holds the directory the command runs in, the prompt glyph,
//! the input with its caret, and a status at the row's right. An input wider
//! than the row wraps onto more rows under it. The command palette's header is
//! one. Every prompt that runs a command paints its line here, so the lines
//! read alike.

use crate::{
    render::{
        chrome, command_palette,
        paint::style_rgb,
        picker::{
            blank_plate_row, plate_chip_boxes, plate_run, plate_trailing, window_start,
            PlateTrailing, MIN_QUERY_COLUMNS, PLATE_HEADER_GAP, PLATE_HEADER_NUDGE,
        },
        run_pane::StatusPart,
        text::{pre_wrap_rows, write_str, write_str_clipped},
        TypeScale,
    },
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use std::ops::Range;
use stoat_widgets::{
    text_run::{self, TextRun},
    ApcScene,
};
use unicode_width::UnicodeWidthStr;

/// The glyph between a Cmd Line's cwd and its input.
pub(crate) const PROMPT_GLYPH: &str = "\u{276f}";

/// The most rows a Cmd Line takes, so that a pasted line leaves the list under
/// it in view.
pub(crate) const MAX_CMD_ROWS: u16 = 4;

/// What a Cmd Line shows at its first row's right end.
pub(crate) enum CmdStatus<'a> {
    /// No status, as over a free-typed argument, which lists nothing to count.
    None,
    /// Styled spans set flush right, such as the count of the list below.
    Text(&'a [(String, Style)]),
    /// A chip that names the mode of the body below, such as the place of a
    /// history walk.
    ///
    /// It draws in `ui.preview.chip`, as a filter header draws its chip.
    Chip(&'a str),
    /// Parts that end at the row's right end in order, such as a Run Block's
    /// mark and runtime.
    ///
    /// Each part draws as words or as a Source Tag in its own style, so the
    /// status holds a tag beside colored words, which neither `Text` nor
    /// `Chip` holds together.
    Parts(&'a [StatusPart]),
}

/// The parts of one Cmd Line, for [`cmd_line`].
pub(crate) struct CmdLine<'a> {
    /// The directory the command runs in, already abbreviated, or empty for a
    /// row that names none.
    pub(crate) cwd: &'a str,
    /// What the user typed, as styled spans that read in order as one text.
    pub(crate) input: &'a [(String, Style)],
    /// The char index of the caret into the joined input, or `None` for a row
    /// that draws no caret.
    pub(crate) caret: Option<usize>,
    /// The words at the first row's right end.
    pub(crate) status: CmdStatus<'a>,
}

/// Chars of the input in one style, as a row of the input shows them.
struct Piece<'a> {
    text: &'a str,
    style: Style,
    /// Whether the piece is the caret's char, which draws in the caret's look.
    caret: bool,
}

/// Paint `line` into `inner`'s first rows, and rule the row under them off
/// from the body.
///
/// `inner` is a modal's rect inside its border. The line takes the rows that
/// [`cmd_line_rows`] counts, and a caller's body starts one row under them.
/// The cwd leads in `ui.cmd.cwd`, the [`PROMPT_GLYPH`] follows in
/// `ui.cmd.prompt`, and the input follows the glyph. The status ends at the
/// first row's right. A row too narrow for the cwd, the glyph,
/// [`MIN_QUERY_COLUMNS`] of input, and the status drops the status first and
/// the cwd second, because the input is the part the user types in.
///
/// A chip status draws in `ui.preview.chip`, as a filter header draws its chip.
/// Under the plate it is a rounded box 6 px wider than its run at each side,
/// which ends 2 px inside the row's right, as [`plate_trailing`] places a chip.
/// In cells its text stands between two blank cells. A theme without the scope
/// draws the chip's text in `ui.text.muted`.
///
/// A parts status ends at the first row's right in order. In a live scene the
/// parts stand [`PLATE_HEADER_GAP`] apart and the input ends that gap before
/// the first one. A part with `chip` set draws as a Source Tag, and any other
/// part as a [`TypeRole::Caption`] run in its foreground over no background.
/// In cells each part writes its text in its style, one blank cell apart. A
/// row too narrow for the parts drops them together.
///
/// The input wraps as [`pre_wrap_rows`] breaks it, and each of its rows spans
/// from the input's column to the status's slot. The cwd, the glyph, and the
/// status stay on the first row. An input of more than [`MAX_CMD_ROWS`] rows
/// shows the rows around its caret.
///
/// Under the plate look of [`plate_colors`](chrome::plate_colors), the rows
/// draw as runs on the plate's ground, [`PLATE_HEADER_NUDGE`] px off their
/// cells, and no rule draws under them. The parts stand [`PLATE_HEADER_GAP`]
/// apart at the [`TypeRole::Ui`] size, the status at the [`TypeRole::Caption`]
/// size, and the caret's char draws in the ground over the `ui.text` color.
/// Otherwise the parts draw in cells one blank cell apart, the caret's cell
/// takes [`Theme::cursor_style`], and a rule in `ui.border.inactive` fills the
/// row under the input. A part whose color has no RGB value draws no run.
pub(crate) fn cmd_line(
    buf: &mut Buffer,
    inner: Rect,
    line: &CmdLine<'_>,
    theme: &Theme,
    scene: &mut ApcScene,
) {
    if inner.is_empty() {
        return;
    }
    if let Some(plate) = chrome::plate_colors(theme, scene) {
        paint_plate_rows(buf, inner, line, plate.ground, theme, scene);
        return;
    }

    let rows = paint_cell_rows(buf, inner, line, theme, scene);
    let rule = inner.y.saturating_add(rows);
    if rule < inner.bottom() {
        chrome::hline(
            buf,
            inner.x,
            rule,
            inner.width,
            theme.get(scope::UI_BORDER_INACTIVE),
            scene,
        );
    }
}

/// The rows that the Cmd Line of `line` takes in a rect `width` cells wide,
/// as [`cmd_line`] paints it into `scene`, from 1 to [`MAX_CMD_ROWS`].
///
/// The painter breaks the input by this same measure, so a caller that lays a
/// body out under the line reads the rows that the paint takes.
pub(crate) fn cmd_line_rows(
    line: &CmdLine<'_>,
    width: u16,
    theme: &Theme,
    scene: &ApcScene,
) -> u16 {
    let room = match chrome::plate_colors(theme, scene) {
        Some(_) => plate_measure(line, width, theme).room,
        None => cell_measure(line, width, theme, scene.live()).room,
    };
    shown_rows(line, room).len() as u16
}

/// Paint `line` on the one row `row` of a pane, such as a Run Block's header.
///
/// The row reads as the first row of [`cmd_line`] under a theme with no plate
/// colors. The cwd, the glyph, and the input stand one blank cell apart in
/// cells, and the status ends at the row's right end. A pane's row stands on
/// the pane's surface, and the row under it is the pane's output, so the row
/// blanks no cell to a ground and rules no row.
///
/// The input shows the row that holds its caret, or its first row with no
/// caret, so a line wider than the row is cut before the status. A parts
/// status draws as [`cmd_line`] draws it, and a text or chip status draws in
/// cells.
pub(crate) fn cmd_line_row(
    buf: &mut Buffer,
    row: Rect,
    line: &CmdLine<'_>,
    theme: &Theme,
    scene: &mut ApcScene,
) {
    if row.is_empty() {
        return;
    }
    let row = Rect { height: 1, ..row };
    let measure = cell_measure(line, row.width, theme, scene.live());
    paint_cell_lead(buf, row, line, &measure, theme, scene);

    let input_x = row.x + measure.input_col;
    let input_end = input_x.saturating_add(u16::try_from(measure.room).unwrap_or(u16::MAX));
    let chars = caret_row(line, measure.room);
    write_input_row(buf, (input_x, row.y), input_end, line, chars, theme);
}

/// The input of `line` as Cmd Line spans, its first `token_chars` chars as
/// the command token.
///
/// The token takes `ui.cmd.command`, or `ui.text` where the theme leaves that
/// unset, and the rest takes `ui.text`. An empty side gives no span.
pub(crate) fn token_spans(text: &str, token_chars: usize, theme: &Theme) -> Vec<(String, Style)> {
    let split = text
        .char_indices()
        .nth(token_chars)
        .map_or(text.len(), |(at, _)| at);
    let (token, rest) = text.split_at(split);
    let text_style = theme.get(scope::UI_TEXT);
    let token_style = theme.try_get(scope::UI_CMD_COMMAND).unwrap_or(text_style);
    [(token, token_style), (rest, text_style)]
        .into_iter()
        .filter(|(part, _)| !part.is_empty())
        .map(|(part, style)| (part.to_owned(), style))
        .collect()
}

/// Where the parts of a cell row stand, in cells from the row's left.
struct CellMeasure {
    show_cwd: bool,
    /// Whether the row keeps the status, which ends at the row's right end.
    show_status: bool,
    /// The input's first cell.
    input_col: u16,
    /// The chars of input that one row holds.
    room: usize,
}

/// Where the parts of a row `width` cells wide stand in the cell look, in a
/// scene that is `live` or not.
///
/// The cwd, the glyph, and the input stand one blank cell apart. The status
/// ends flush with the row's right end, as far clear of the input as
/// [`status_trail`] counts.
fn cell_measure(line: &CmdLine<'_>, width: u16, theme: &Theme, live: bool) -> CellMeasure {
    let trail = status_trail(&line.status, theme, live);
    let cwd_width = line.cwd.width() as u16;
    let glyph_width = PROMPT_GLYPH.width() as u16;

    let lead = |cwd: bool| match cwd && cwd_width > 0 {
        true => cwd_width + 1 + glyph_width + 1,
        false => glyph_width + 1,
    };
    let fits = |cwd: bool, status: bool| {
        let trail = match status {
            true => trail,
            false => 0,
        };
        u32::from(lead(cwd)) + u32::from(MIN_QUERY_COLUMNS) + u32::from(trail) <= u32::from(width)
    };
    let (show_cwd, show_status) = parts_kept(fits);

    let show_status = show_status && trail > 0;
    let input_col = lead(show_cwd).min(width);
    let input_end = match show_status {
        true => width.saturating_sub(trail),
        false => width,
    };
    CellMeasure {
        show_cwd,
        show_status,
        input_col,
        room: usize::from(input_end.saturating_sub(input_col)),
    }
}

/// Where the parts of a plate row stand, in sixteenths from the row's left.
struct PlateMeasure {
    show_cwd: bool,
    /// Whether the row keeps the status.
    show_status: bool,
    glyph_x: u16,
    input_x: u16,
    /// The status's place, which [`plate_trailing`] gives as a count's.
    trailing: PlateTrailing,
    /// The chars of input that one row holds.
    room: usize,
}

/// Where the parts of a row `width` cells wide stand in the plate look.
///
/// The cwd starts at the row's left, the glyph and the input follow
/// [`PLATE_HEADER_GAP`] apart, and the status ends at the row's right, as
/// [`plate_trailing`] places a count.
fn plate_measure(line: &CmdLine<'_>, width: u16, theme: &Theme) -> PlateMeasure {
    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let advance = |text: &str, scale| text_run::advance_sixteenths(text.chars().count(), scale);
    let status: String = status_spans(&line.status)
        .iter()
        .map(|(text, _)| text.as_str())
        .collect();
    let chip = match line.status {
        CmdStatus::Chip(chip) => Some(chip),
        _ => None,
    };
    let parts = match line.status {
        CmdStatus::Parts(parts) if !parts.is_empty() => Some(parts),
        _ => None,
    };
    let boxed = theme
        .try_get(scope::UI_PREVIEW_CHIP)
        .and_then(|style| style_rgb(style.bg))
        .is_some();
    let end = width.saturating_mul(16);
    let status_trail = match (chip, parts) {
        (Some(chip), _) => {
            let alone = plate_trailing(None, &[(chip, true)], boxed, end, 0, caption);
            end.saturating_sub(alone.query_end)
        },
        (None, Some(parts)) => parts_span(parts, theme).saturating_add(PLATE_HEADER_GAP),
        (None, None) if status.is_empty() => 0,
        (None, None) => advance(&status, caption).saturating_add(PLATE_HEADER_GAP),
    };

    let glyph_at = |cwd: bool| match cwd && !line.cwd.is_empty() {
        true => advance(line.cwd, ui).saturating_add(PLATE_HEADER_GAP),
        false => 0,
    };
    let input_at = |cwd: bool| {
        glyph_at(cwd)
            .saturating_add(advance(PROMPT_GLYPH, ui))
            .saturating_add(PLATE_HEADER_GAP)
    };
    let query_min = |cwd: bool| input_at(cwd).saturating_add(MIN_QUERY_COLUMNS * 16);
    let fits = |cwd: bool, shown: bool| {
        let trail = match shown {
            true => status_trail,
            false => 0,
        };
        u32::from(query_min(cwd)) + u32::from(trail) <= u32::from(end)
    };
    let (show_cwd, show_status) = parts_kept(fits);
    let trailing = match (chip.filter(|_| show_status), parts) {
        (Some(chip), _) => plate_trailing(
            None,
            &[(chip, true)],
            boxed,
            end,
            query_min(show_cwd),
            caption,
        ),
        (None, Some(_)) => PlateTrailing {
            count: None,
            chips: Vec::new(),
            query_end: match show_status {
                true => end.saturating_sub(status_trail),
                false => end,
            },
        },
        (None, None) => {
            let shown = Some(status.as_str()).filter(|status| show_status && !status.is_empty());
            plate_trailing(shown, &[], false, end, query_min(show_cwd), caption)
        },
    };

    let input_x = input_at(show_cwd);
    let room =
        usize::from(trailing.query_end.saturating_sub(input_x)) * 16 / usize::from(ui.max(1));
    PlateMeasure {
        show_cwd,
        show_status,
        glyph_x: glyph_at(show_cwd),
        input_x,
        trailing,
        room,
    }
}

/// Paint `line` into `inner`'s first rows as cells, for [`cmd_line`], and
/// answer how many rows the input takes.
fn paint_cell_rows(
    buf: &mut Buffer,
    inner: Rect,
    line: &CmdLine<'_>,
    theme: &Theme,
    scene: &mut ApcScene,
) -> u16 {
    let measure = cell_measure(line, inner.width, theme, scene.live());
    paint_cell_lead(
        buf,
        Rect { height: 1, ..inner },
        line,
        &measure,
        theme,
        scene,
    );

    let input_x = inner.x + measure.input_col;
    let input_end = input_x.saturating_add(u16::try_from(measure.room).unwrap_or(u16::MAX));
    let rows = shown_rows(line, measure.room);
    for (row_y, row) in (inner.y..inner.bottom()).zip(&rows) {
        write_input_row(buf, (input_x, row_y), input_end, line, row.clone(), theme);
    }
    (rows.len() as u16).min(inner.height)
}

/// Write the cwd, the glyph, and the status of `line` on the cell row `row`,
/// where `measure` places them.
fn paint_cell_lead(
    buf: &mut Buffer,
    row: Rect,
    line: &CmdLine<'_>,
    measure: &CellMeasure,
    theme: &Theme,
    scene: &mut ApcScene,
) {
    let (y, end) = (row.y, row.right());
    let mut x = row.x;
    if measure.show_cwd && !line.cwd.is_empty() {
        write_str_clipped(buf, x, y, line.cwd, cwd_style(theme), end);
        x = x.saturating_add(line.cwd.width() as u16 + 1);
    }
    write_str_clipped(buf, x, y, PROMPT_GLYPH, prompt_style(theme), end);
    if !measure.show_status {
        return;
    }

    if let CmdStatus::Parts(parts) = line.status {
        paint_parts(buf, row, parts, theme, scene);
        return;
    }
    let mut at = end.saturating_sub(status_cells(&line.status));
    if let CmdStatus::Chip(chip) = line.status {
        let text = format!(" {chip} ");
        let cells = Rect::new(at, y, text.width() as u16, 1);
        let style = match theme.try_get(scope::UI_PREVIEW_CHIP) {
            Some(style) => chrome::chip_cells(cells, style, scene),
            None => theme.get(scope::UI_TEXT_MUTED),
        };
        write_str(buf, at, y, &text, style);
    }
    for (text, style) in status_spans(&line.status) {
        write_str(buf, at, y, text, *style);
        at += text.width() as u16;
    }
}

/// Write the chars `chars` of `line`'s input from `(x, y)`, cut at the cell
/// `end`, with the caret's char in [`Theme::cursor_style`].
fn write_input_row(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    end: u16,
    line: &CmdLine<'_>,
    chars: Range<usize>,
    theme: &Theme,
) {
    let mut at = x;
    for piece in row_pieces(line, chars) {
        let style = match piece.caret {
            true => theme.cursor_style(),
            false => piece.style,
        };
        write_str_clipped(buf, at, y, piece.text, style, end);
        at = at.saturating_add(piece.text.width() as u16);
    }
}

/// Paint `parts` so they end at `row`'s right end, in order.
///
/// A live scene draws them [`PLATE_HEADER_GAP`] apart. A part with `chip` set
/// draws as a Source Tag, and any other part as a [`TypeRole::Caption`] run in
/// its foreground over no background, or as no run where that color has no
/// RGB value. Otherwise each part writes its text in cells in its style, one
/// blank cell apart.
fn paint_parts(
    buf: &mut Buffer,
    row: Rect,
    parts: &[StatusPart],
    theme: &Theme,
    scene: &mut ApcScene,
) {
    if !scene.live() {
        let mut x = row.right().saturating_sub(parts_cells(parts));
        for part in parts {
            write_str_clipped(buf, x, row.y, &part.text, part.style, row.right());
            x = x.saturating_add(part.text.width() as u16 + 1);
        }
        return;
    }

    let caption = theme.type_scale(TypeRole::Caption);
    let mut col = row
        .width
        .saturating_mul(16)
        .saturating_sub(parts_span(parts, theme));
    for part in parts {
        if part.chip {
            command_palette::paint_source_tag(buf, row, col, &part.text, part.style, theme, scene);
        } else if let Some(color) = style_rgb(part.style.fg) {
            let run = TextRun {
                col: col as i16,
                row: 0,
                scale: caption,
                color,
                bg: None,
                text: &part.text,
                bold: part.style.add_modifier.contains(Modifier::BOLD),
                italic: false,
                follow: 0,
                anchor: None,
            };
            run.render(row, buf, scene);
        }
        col = col.saturating_add(part_width(part, theme).saturating_add(PLATE_HEADER_GAP));
    }
}

/// Paint `line` into `inner`'s first rows as a plate's header, runs on the
/// plate's `ground`, for [`cmd_line`].
///
/// Each header row blanks to the ground. The cwd, the glyph, and the status
/// draw on the first row, and each row of the input draws at the input's
/// column, each run where [`text_run::advance_sixteenths`] places its first
/// char in the row.
fn paint_plate_rows(
    buf: &mut Buffer,
    inner: Rect,
    line: &CmdLine<'_>,
    ground: [u8; 3],
    theme: &Theme,
    scene: &mut ApcScene,
) {
    let measure = plate_measure(line, inner.width, theme);
    let rows = shown_rows(line, measure.room);
    let height = (rows.len() as u16).min(inner.height);
    for y in inner.y..inner.y + height {
        blank_plate_row(buf, Rect::new(inner.x, y, inner.width, 1), ground);
    }

    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let text = style_rgb(theme.get(scope::UI_TEXT).fg);
    let bold = |style: Style| style.add_modifier.contains(Modifier::BOLD);
    let chip_style = theme.try_get(scope::UI_PREVIEW_CHIP);
    let fill = chip_style.and_then(|style| style_rgb(style.bg));

    scene.nudge(PLATE_HEADER_NUDGE);
    if let Some(fill) = fill {
        plate_chip_boxes(&measure.trailing, fill, inner, buf, scene);
    }
    let mut run =
        |y: u16, col: u16, part: &str, scale: u16, colors: (Option<[u8; 3]>, [u8; 3]), bold| {
            if let (Some(color), false) = (colors.0, part.is_empty()) {
                let origin = Rect::new(inner.x, y, 1, 1);
                plate_run(col, part, scale, color, colors.1, bold).render(origin, buf, scene);
            }
        };

    if measure.show_cwd {
        let cwd = cwd_style(theme);
        run(
            inner.y,
            0,
            line.cwd,
            ui,
            (style_rgb(cwd.fg), ground),
            bold(cwd),
        );
    }
    let prompt = prompt_style(theme);
    run(
        inner.y,
        measure.glyph_x,
        PROMPT_GLYPH,
        ui,
        (style_rgb(prompt.fg), ground),
        bold(prompt),
    );

    for (y, row) in (inner.y..inner.y + height).zip(&rows) {
        let mut index = 0;
        for piece in row_pieces(line, row.clone()) {
            let col = measure
                .input_x
                .saturating_add(text_run::advance_sixteenths(index, ui));
            match (piece.caret, text) {
                (true, Some(text)) => run(y, col, piece.text, ui, (Some(ground), text), false),
                (true, None) => {},
                (false, _) => run(
                    y,
                    col,
                    piece.text,
                    ui,
                    (style_rgb(piece.style.fg), ground),
                    bold(piece.style),
                ),
            }
            index += piece.text.chars().count();
        }
    }

    if let Some(mut col) = measure.trailing.count {
        for (part, style) in status_spans(&line.status) {
            run(
                inner.y,
                col,
                part,
                caption,
                (style_rgb(style.fg), ground),
                bold(*style),
            );
            col = col.saturating_add(text_run::advance_sixteenths(part.chars().count(), caption));
        }
    }
    if let CmdStatus::Chip(chip) = line.status
        && let Some((col, cells)) = measure.trailing.chips.first()
    {
        let colors = match (chip_style, cells) {
            (Some(style), Some(_)) => (style_rgb(style.fg), fill.unwrap_or(ground)),
            (Some(style), None) => (style_rgb(style.fg), ground),
            (None, _) => (style_rgb(theme.get(scope::UI_TEXT_MUTED).fg), ground),
        };
        run(inner.y, *col, chip, caption, colors, false);
    }
    if let CmdStatus::Parts(parts) = line.status
        && measure.show_status
    {
        let row = Rect::new(inner.x, inner.y, inner.width, 1);
        paint_parts(buf, row, parts, theme, scene);
    }
    scene.nudge([0, 0]);
}

/// Which of the cwd and the status a row keeps, as `(cwd, status)`.
///
/// `fits` answers whether the row holds the input's minimum with the cwd and
/// the status it is asked about. The status drops first and the cwd second,
/// and a status the row drops stays dropped when the cwd goes too.
fn parts_kept(fits: impl Fn(bool, bool) -> bool) -> (bool, bool) {
    match (fits(true, true), fits(true, false)) {
        (true, _) => (true, true),
        (false, true) => (true, false),
        (false, false) => (false, false),
    }
}

/// The spans of `status`, empty for a row with none and for a chip or parts,
/// which draw apart from the spans.
fn status_spans<'a>(status: &CmdStatus<'a>) -> &'a [(String, Style)] {
    match status {
        CmdStatus::None | CmdStatus::Chip(_) | CmdStatus::Parts(_) => &[],
        CmdStatus::Text(spans) => spans,
    }
}

/// The cells that `status` takes in the cell look. A chip takes a blank cell
/// on each side of its text, as a filter header's chip does, and parts stand
/// one blank cell apart.
fn status_cells(status: &CmdStatus<'_>) -> u16 {
    match status {
        CmdStatus::Chip(chip) => chip.width() as u16 + 2,
        CmdStatus::Parts(parts) => parts_cells(parts),
        _ => status_spans(status)
            .iter()
            .map(|(text, _)| text.width() as u16)
            .sum(),
    }
}

/// The cells that `status` takes at a cell row's right end with the gap
/// before it, in a scene that is `live` or not, or 0 for a row with none.
///
/// The gap is one blank cell, except before the parts of a live scene. Those
/// draw as runs [`PLATE_HEADER_GAP`] clear of the input, so they take each
/// cell that they and the gap reach into.
fn status_trail(status: &CmdStatus<'_>, theme: &Theme, live: bool) -> u16 {
    match status {
        CmdStatus::Parts(parts) if live && !parts.is_empty() => parts_span(parts, theme)
            .saturating_add(PLATE_HEADER_GAP)
            .div_ceil(16),
        _ => match status_cells(status) {
            0 => 0,
            cells => cells + 1,
        },
    }
}

/// The cells that `parts` take in cells, one blank cell apart.
fn parts_cells(parts: &[StatusPart]) -> u16 {
    let text: u16 = parts.iter().map(|part| part.text.width() as u16).sum();
    text.saturating_add((parts.len() as u16).saturating_sub(1))
}

/// The sixteenths that `parts` span as runs, [`PLATE_HEADER_GAP`] apart.
fn parts_span(parts: &[StatusPart], theme: &Theme) -> u16 {
    let widths: u16 = parts.iter().map(|part| part_width(part, theme)).sum();
    let gaps = (parts.len() as u16).saturating_sub(1);
    widths.saturating_add(gaps.saturating_mul(PLATE_HEADER_GAP))
}

/// The sixteenths that `part` takes as a run, or as a Source Tag's box for a
/// part with `chip` set.
fn part_width(part: &StatusPart, theme: &Theme) -> u16 {
    match part.chip {
        true => command_palette::source_tag_width(&part.text, theme),
        false => text_run::advance_sixteenths(
            part.text.chars().count(),
            theme.type_scale(TypeRole::Caption),
        ),
    }
}

/// The rows of `line`'s input that the line shows when a row holds `room`
/// chars, as [`pre_wrap_rows`] breaks them, each a range of char indices.
///
/// A caret past the input's end takes a cell of its own. An input of more than
/// [`MAX_CMD_ROWS`] rows shows the rows that [`window_start`] picks around the
/// caret's row, or around the last row of a line with no caret.
fn shown_rows(line: &CmdLine<'_>, room: usize) -> Vec<Range<usize>> {
    let cells: Vec<char> = line
        .input
        .iter()
        .flat_map(|(text, _)| text.chars())
        .collect();
    let caret = line.caret.map(|caret| caret.min(cells.len()));
    let rows = pre_wrap_rows(&cells, room, caret == Some(cells.len()));

    let caret_row = caret
        .and_then(|caret| rows.iter().position(|row| row.contains(&caret)))
        .unwrap_or(rows.len() - 1);
    let first = window_start(caret_row, usize::from(MAX_CMD_ROWS));
    rows.into_iter()
        .skip(first)
        .take(usize::from(MAX_CMD_ROWS))
        .collect()
}

/// The chars of `line`'s input on the row that holds its caret, or on its
/// first row for a line with no caret, as [`pre_wrap_rows`] breaks the input
/// into rows of `room` chars.
fn caret_row(line: &CmdLine<'_>, room: usize) -> Range<usize> {
    let cells: Vec<char> = line
        .input
        .iter()
        .flat_map(|(text, _)| text.chars())
        .collect();
    let caret = line.caret.map(|caret| caret.min(cells.len()));
    let rows = pre_wrap_rows(&cells, room, caret == Some(cells.len()));

    let at = caret
        .and_then(|caret| rows.iter().position(|row| row.contains(&caret)))
        .unwrap_or(0);
    rows.into_iter().nth(at).unwrap_or_default()
}

/// The pieces of `line`'s input in the cells of `row`, as [`spans_between`]
/// gives them, with the cell of a caret past the input's end when `row`
/// holds it.
fn row_pieces<'a>(line: &CmdLine<'a>, row: Range<usize>) -> Vec<Piece<'a>> {
    let len: usize = line
        .input
        .iter()
        .map(|(text, _)| text.chars().count())
        .sum();
    let caret = line.caret.map(|caret| caret.min(len));
    let mut pieces = spans_between(line.input, row.start..row.end.min(len), caret);
    if caret == Some(len) && row.contains(&len) {
        pieces.push(Piece {
            text: " ",
            style: Style::default(),
            caret: true,
        });
    }
    pieces
}

/// The chars of `spans` from `window.start` up to `window.end`, as pieces of
/// one style each, where the char at `caret` stands alone.
fn spans_between<'a>(
    spans: &'a [(String, Style)],
    window: Range<usize>,
    caret: Option<usize>,
) -> Vec<Piece<'a>> {
    let mut pieces = Vec::new();
    let mut first = 0;
    for (text, style) in spans {
        let chars = first..first + text.chars().count();
        first = chars.end;

        let mut cuts = vec![chars.start.max(window.start), chars.end.min(window.end)];
        if let Some(caret) = caret.filter(|caret| (cuts[0]..cuts[1]).contains(caret)) {
            cuts.splice(1..1, [caret, caret + 1]);
        }
        let byte = |index: usize| {
            text.char_indices()
                .nth(index - chars.start)
                .map_or(text.len(), |(at, _)| at)
        };
        for pair in cuts.windows(2).filter(|pair| pair[0] < pair[1]) {
            pieces.push(Piece {
                text: &text[byte(pair[0])..byte(pair[1])],
                style: *style,
                caret: caret == Some(pair[0]),
            });
        }
    }
    pieces
}

/// The style of a Cmd Line's cwd, `ui.cmd.cwd`, or `ui.key_label` where a theme
/// leaves it unset.
fn cwd_style(theme: &Theme) -> Style {
    theme
        .try_get(scope::UI_CMD_CWD)
        .unwrap_or_else(|| theme.get(scope::UI_KEY_LABEL))
}

/// The style of a Cmd Line's prompt glyph, `ui.cmd.prompt`, or `ui.text.dim`
/// where a theme leaves it unset.
fn prompt_style(theme: &Theme) -> Style {
    theme
        .try_get(scope::UI_CMD_PROMPT)
        .unwrap_or_else(|| theme.get(scope::UI_TEXT_DIM))
}

#[cfg(test)]
mod tests {
    use super::{
        cmd_line, cmd_line_row, cmd_line_rows, token_spans, CmdLine, CmdStatus, PROMPT_GLYPH,
    };
    use crate::{
        render::run_pane::StatusPart,
        test_fixture,
        theme::{scope, Theme},
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command, TextRunCommand};

    const GROUND: [u8; 3] = [1, 1, 1];
    const TEXT: [u8; 3] = [4, 4, 4];
    const MUTED: [u8; 3] = [5, 5, 5];
    const CWD: [u8; 3] = [10, 10, 10];
    const PROMPT: [u8; 3] = [11, 11, 11];

    /// A theme with the plate's two surfaces, the text colors, and the Cmd
    /// Line's own colors, whose plate needs a scene that reads nudges.
    fn plate_theme() -> Theme {
        let src = r##"theme t {
            ui.surface.ground.bg = "#010101";
            ui.background.bg = "#020202";
            ui.text.fg = "#040404";
            ui.text.muted.fg = "#050505";
            ui.cmd.cwd.fg = "#0a0a0a";
            ui.cmd.prompt.fg = "#0b0b0b";
            ui.preview.chip = { fg: "#0c0c0c", bg: "#0d0d0d" };
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// A theme that sets only the rule's color, so the cell look draws its
    /// rule as a bar and its caret in the reversed fallback.
    fn rule_theme() -> Theme {
        let (config, _) =
            stoat_config::parse(r##"theme t { ui.border.inactive.fg = "#090909"; }"##);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The parts of a line, as its cwd, its input, its caret, and its status.
    type Parts<'a> = (&'a str, &'a str, Option<usize>, &'a str);

    /// Paint the line of `parts` into `inner` of a 64 by 6 buffer under
    /// `theme`, by a scene that reads nudges where `nudges` holds.
    fn painted(parts: Parts<'_>, inner: Rect, theme: &Theme, nudges: bool) -> (Buffer, ApcScene) {
        with_line(parts, theme, |line| paint_line(line, inner, theme, nudges))
    }

    /// Paint `line` into `inner` of a 64 by 6 buffer under `theme`, by a scene
    /// that reads nudges where `nudges` holds.
    fn paint_line(
        line: &CmdLine<'_>,
        inner: Rect,
        theme: &Theme,
        nudges: bool,
    ) -> (Buffer, ApcScene) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 64, 6));
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(nudges);
        cmd_line(&mut buf, inner, line, theme, &mut scene);
        (buf, scene)
    }

    /// Hand `f` the line of `cwd`, the input `input` with its caret at `caret`,
    /// and the muted `status` under `theme`.
    fn with_line<R>(
        (cwd, input, caret, status): Parts<'_>,
        theme: &Theme,
        f: impl FnOnce(&CmdLine<'_>) -> R,
    ) -> R {
        let input = [(input.to_owned(), theme.get(scope::UI_TEXT))];
        let spans = [(status.to_owned(), theme.get(scope::UI_TEXT_MUTED))];
        f(&CmdLine {
            cwd,
            input: &input,
            caret,
            status: match status.is_empty() {
                true => CmdStatus::None,
                false => CmdStatus::Text(&spans),
            },
        })
    }

    /// The commands and the rows of the line of `input` with its caret at
    /// `caret` under the cwd `~/p/stoat` and the status `14 of 890`, painted
    /// under the plate into inner (2, 1, 60, 4), where a row holds 38 chars.
    fn wrapped(input: &str, caret: usize) -> (Vec<Command>, u16) {
        let parts = ("~/p/stoat", input, Some(caret), "14 of 890");
        let theme = plate_theme();
        let (_, scene) = painted(parts, Rect::new(2, 1, 60, 4), &theme, true);
        let rows = with_line(parts, &theme, |line| {
            cmd_line_rows(line, 60, &theme, &scene)
        });
        (commands(&scene), rows)
    }

    /// The commands of a line that [`wrapped`] paints, with `input` as its
    /// input runs, between the cwd and glyph runs and the status run.
    fn framed(input: Vec<Command>) -> Vec<Command> {
        let mut commands = vec![
            Command::Nudge([1, 7]),
            run(32, "~/p/stoat", CWD, GROUND),
            run(190, PROMPT_GLYPH, PROMPT, GROUND),
        ];
        commands.extend(input);
        commands.extend([run(844, "14 of 890", MUTED, GROUND), Command::Nudge([0, 0])]);
        commands
    }

    /// A run of `text` at `col` sixteenths on row 1, in `color` over `bg`.
    fn run(col: i16, text: &str, color: [u8; 3], bg: [u8; 3]) -> Command {
        run_on(16, col, text, color, bg)
    }

    /// A run of `text` at `col` and `row` sixteenths, in `color` over `bg`.
    fn run_on(row: i16, col: i16, text: &str, color: [u8; 3], bg: [u8; 3]) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row,
            scale: 256,
            color,
            bg: Some(bg),
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    /// The commands of a scene, less the ends that close its runs.
    fn commands(scene: &ApcScene) -> Vec<Command> {
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// The text of `buf`'s row `y`, with trailing blanks kept.
    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// Under the plate the row is runs 1 px right and 7 px down of its cells.
    /// The cwd leads in its color, the glyph and the input follow 14
    /// sixteenths apart, the caret is a space after `ab` in the ground over the
    /// text color, and the muted status ends at the row's right.
    #[test]
    fn a_plate_cmd_line_draws_its_parts_as_nudged_runs() {
        let (_, scene) = painted(
            ("~/p/stoat", "ab", Some(2), "14 of 890"),
            Rect::new(2, 1, 60, 4),
            &plate_theme(),
            true,
        );

        assert_eq!(
            commands(&scene),
            [
                Command::Nudge([1, 7]),
                run(32, "~/p/stoat", CWD, GROUND),
                run(190, PROMPT_GLYPH, PROMPT, GROUND),
                run(220, "ab", TEXT, GROUND),
                run(252, " ", GROUND, TEXT),
                run(844, "14 of 890", MUTED, GROUND),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// A chip status draws its box as a rounded bar and its run 11 sixteenths
    /// inside it, in the chip's colors. The chip `history 1 / 24` advances 224
    /// sixteenths, so its bar is 246 wide. It ends at 988, 4 sixteenths inside
    /// the right of the 60-cell row that starts at 32.
    #[test]
    fn a_chip_status_draws_its_box_and_its_run() {
        let theme = plate_theme();
        let input = [("ab".to_owned(), theme.get(scope::UI_TEXT))];
        let line = CmdLine {
            cwd: "~/p/stoat",
            input: &input,
            caret: Some(2),
            status: CmdStatus::Chip("history 1 / 24"),
        };
        let (_, scene) = paint_line(&line, Rect::new(2, 1, 60, 4), &theme, true);

        let decoded = commands(&scene);
        let bars: Vec<(i16, u16, u8)> = decoded
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => Some((bar.x, bar.width, bar.radius)),
                _ => None,
            })
            .collect();
        let chip = decoded.iter().find_map(|command| match command {
            Command::TextRun(run) if run.text == "history 1 / 24" => {
                Some((run.col, run.color, run.bg))
            },
            _ => None,
        });
        assert_eq!(
            (bars, chip),
            (
                vec![(742, 246, 4)],
                Some((753, [12, 12, 12], Some([13, 13, 13])))
            ),
        );
    }

    /// In cells the parts stand one blank cell apart, the status ends at the
    /// row's right, the caret's cell takes the reversed fallback, and a rule
    /// fills the second row.
    #[test]
    fn a_cell_cmd_line_writes_its_parts_one_cell_apart() {
        let (buf, scene) = painted(
            ("~/p/stoat", "ab", Some(2), "14 of 890"),
            Rect::new(0, 0, 60, 2),
            &rule_theme(),
            false,
        );

        let caret = buf[(14, 0)].modifier.contains(Modifier::REVERSED);
        assert_eq!(
            (&row(&buf, 0)[..], caret, test_fixture::row_rules(&scene, 1)),
            (
                format!("~/p/stoat \u{276f} ab{}14 of 890    ", " ".repeat(37)).as_str(),
                true,
                1,
            ),
        );
    }

    /// A theme that leaves the Cmd Line's scopes unset, as default_dark does,
    /// paints the cwd in `ui.key_label` and the glyph in `ui.text.dim`.
    #[test]
    fn a_theme_without_the_cmd_scopes_paints_the_fallback_colors() {
        let theme = {
            let src = r##"theme t {
                ui.key_label.fg = "#0c0c0c";
                ui.text.dim.fg = "#0d0d0d";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let (buf, _) = painted(
            ("~/p/stoat", "ab", Some(2), ""),
            Rect::new(0, 0, 60, 2),
            &theme,
            false,
        );

        assert_eq!(
            [buf[(0, 0)].fg, buf[(10, 0)].fg],
            [Color::Rgb(12, 12, 12), Color::Rgb(13, 13, 13)],
        );
    }

    /// A caret inside the input marks its own char, and the chars on each side
    /// of it keep their style.
    #[test]
    fn a_caret_inside_the_input_marks_its_own_char() {
        let (buf, _) = painted(
            ("", "abc", Some(1), ""),
            Rect::new(0, 0, 20, 2),
            &Theme::empty(),
            false,
        );

        let reversed = [2, 3, 4].map(|x| buf[(x, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(
            (row(&buf, 0).trim_end(), reversed),
            ("\u{276f} abc", [false, true, false]),
        );
    }

    /// A line with no cwd starts at its glyph, and the input follows it.
    #[test]
    fn an_empty_cwd_starts_the_cmd_line_at_its_glyph() {
        let (_, scene) = painted(
            ("", "ab", Some(2), ""),
            Rect::new(2, 1, 60, 4),
            &plate_theme(),
            true,
        );

        assert_eq!(
            commands(&scene)[1..3],
            [
                run(32, PROMPT_GLYPH, PROMPT, GROUND),
                run(62, "ab", TEXT, GROUND)
            ],
        );
    }

    /// A row too narrow for every part drops the status first and then the
    /// cwd, so the input keeps its 12 columns. The parts need 34 columns, and
    /// the cwd, the glyph, and the input need 24.
    #[test]
    fn a_narrow_cmd_line_drops_its_status_and_then_its_cwd() {
        let kept = |width| {
            let (buf, _) = painted(
                ("~/p/stoat", "ab", Some(2), "14 of 890"),
                Rect::new(0, 0, width, 2),
                &Theme::empty(),
                false,
            );
            let text = row(&buf, 0);
            (text.contains("~/p/stoat"), text.contains("14 of 890"))
        };

        assert_eq!(
            [34, 33, 24, 23].map(kept),
            [(true, true), (true, false), (true, false), (false, false)],
        );
    }

    /// An input wider than its row breaks after the last space that fits, and
    /// the space stays at the end of the first row.
    #[test]
    fn a_long_input_breaks_after_the_last_space_that_fits() {
        let input = format!("{} {}", "a".repeat(30), "b".repeat(20));
        assert_eq!(
            wrapped(&input, 51),
            (
                framed(vec![
                    run_on(16, 220, &format!("{} ", "a".repeat(30)), TEXT, GROUND),
                    run_on(32, 220, &"b".repeat(20), TEXT, GROUND),
                    run_on(32, 540, " ", GROUND, TEXT),
                ]),
                2,
            ),
        );
    }

    /// A word longer than its row breaks at the row's last cell.
    #[test]
    fn a_long_input_wraps_at_the_last_cell_of_its_row() {
        assert_eq!(
            wrapped(&"x".repeat(50), 50),
            (
                framed(vec![
                    run_on(16, 220, &"x".repeat(38), TEXT, GROUND),
                    run_on(32, 220, &"x".repeat(12), TEXT, GROUND),
                    run_on(32, 412, " ", GROUND, TEXT),
                ]),
                2,
            ),
        );
    }

    /// A line that fills its row leaves a caret past its end alone on the next
    /// row, with a space in the line and without one.
    #[test]
    fn a_caret_past_a_full_row_takes_the_next_row() {
        let caret_alone = |input: &str| {
            let input = vec![
                run_on(16, 220, input, TEXT, GROUND),
                run_on(32, 220, " ", GROUND, TEXT),
            ];
            (framed(input), 2)
        };
        let (full, spaced) = (
            "x".repeat(38),
            format!("{} {}", "a".repeat(18), "b".repeat(19)),
        );

        assert_eq!(
            [wrapped(&full, 38), wrapped(&spaced, 38)],
            [caret_alone(&full), caret_alone(&spaced)],
        );
    }

    /// In cells the input wraps at the room of the plate's row, its rows start
    /// at the input's column, and the rule moves under the last row.
    #[test]
    fn a_cell_cmd_line_wraps_at_the_same_room() {
        let (buf, scene) = painted(
            ("~/p/stoat", &"x".repeat(50), Some(50), "14 of 890"),
            Rect::new(0, 0, 60, 4),
            &rule_theme(),
            false,
        );

        let caret = buf[(24, 1)].modifier.contains(Modifier::REVERSED);
        assert_eq!(
            (
                [row(&buf, 0), row(&buf, 1)],
                caret,
                [1, 2].map(|y| test_fixture::row_rules(&scene, y)),
            ),
            (
                [
                    format!("~/p/stoat \u{276f} {} 14 of 890    ", "x".repeat(38)),
                    format!("{}{}{}", " ".repeat(12), "x".repeat(12), " ".repeat(40)),
                ],
                true,
                [0, 1],
            ),
        );
    }

    /// An input of more than four rows shows the four rows that end at the
    /// caret's row, or the first four for a caret on the first row. The cwd,
    /// the glyph, and the status stay on the first row.
    #[test]
    fn a_header_stops_at_four_rows_and_follows_the_caret() {
        let (input, full) = ("x".repeat(200), "x".repeat(38));
        assert_eq!(
            [wrapped(&input, 200), wrapped(&input, 0)],
            [
                (
                    framed(vec![
                        run_on(16, 220, &full, TEXT, GROUND),
                        run_on(32, 220, &full, TEXT, GROUND),
                        run_on(48, 220, &full, TEXT, GROUND),
                        run_on(64, 220, &"x".repeat(10), TEXT, GROUND),
                        run_on(64, 380, " ", GROUND, TEXT),
                    ]),
                    4,
                ),
                (
                    framed(vec![
                        run_on(16, 220, "x", GROUND, TEXT),
                        run_on(16, 236, &"x".repeat(37), TEXT, GROUND),
                        run_on(32, 220, &full, TEXT, GROUND),
                        run_on(48, 220, &full, TEXT, GROUND),
                        run_on(64, 220, &full, TEXT, GROUND),
                    ]),
                    4,
                ),
            ],
        );
    }

    /// Under the plate each row of a wrapped header blanks to the ground, and
    /// the row under the header keeps its cells.
    #[test]
    fn a_wrapped_plate_header_blanks_each_of_its_rows() {
        let (buf, _) = painted(
            ("~/p/stoat", &"x".repeat(50), Some(50), "14 of 890"),
            Rect::new(2, 1, 60, 4),
            &plate_theme(),
            true,
        );

        let blanked = |y: u16| {
            (2..62)
                .filter(|&x| buf[(x, y)].bg == Color::Rgb(1, 1, 1) && buf[(x, y)].symbol() == " ")
                .count()
        };
        assert_eq!([1, 2, 3].map(blanked), [60, 60, 0]);
    }

    /// Under the plate each span of the input draws as its own run in its own
    /// style, as a command token, its ghost, and the argument after them do. A
    /// caret inside a span splits it around the caret's char.
    #[test]
    fn a_plate_cmd_line_draws_each_span_as_its_own_run() {
        let theme = plate_theme();
        let (token, ghost) = ([12; 3], [13; 3]);
        let input = [
            (
                "o".to_owned(),
                Style::default()
                    .fg(Color::Rgb(12, 12, 12))
                    .add_modifier(Modifier::BOLD),
            ),
            (
                "pen".to_owned(),
                Style::default().fg(Color::Rgb(13, 13, 13)),
            ),
            (" x".to_owned(), theme.get(scope::UI_TEXT)),
        ];
        let status = [("14 of 890".to_owned(), theme.get(scope::UI_TEXT_MUTED))];
        let runs = |caret| {
            let line = CmdLine {
                cwd: "~/p/stoat",
                input: &input,
                caret: Some(caret),
                status: CmdStatus::Text(&status),
            };
            commands(&paint_line(&line, Rect::new(2, 1, 60, 4), &theme, true).1)
        };

        assert_eq!(
            [runs(6), runs(1)],
            [
                framed(vec![
                    bold(run(220, "o", token, GROUND)),
                    run(236, "pen", ghost, GROUND),
                    run(284, " x", TEXT, GROUND),
                    run(316, " ", GROUND, TEXT),
                ]),
                framed(vec![
                    bold(run(220, "o", token, GROUND)),
                    run(236, "p", GROUND, TEXT),
                    run(252, "en", ghost, GROUND),
                    run(284, " x", TEXT, GROUND),
                ]),
            ],
        );
    }

    /// `command`, a text run, in the bold face.
    fn bold(command: Command) -> Command {
        match command {
            Command::TextRun(run) => Command::TextRun(TextRunCommand { bold: true, ..run }),
            command => command,
        }
    }

    /// A status part of `text` in the foreground `fg` over `bg`, drawn as a
    /// tag where `chip` holds.
    fn part(text: &str, fg: [u8; 3], bg: Option<[u8; 3]>, chip: bool) -> StatusPart {
        let rgb = |[r, g, b]: [u8; 3]| Color::Rgb(r, g, b);
        let style = Style::default().fg(rgb(fg));
        StatusPart {
            text: text.to_owned(),
            style: bg.map_or(style, |bg| style.bg(rgb(bg))),
            chip,
        }
    }

    /// Paint the line of the cwd `~/p/stoat` and the input `ab` with no caret,
    /// ended by `parts`, through [`cmd_line_row`] on the row (0, 3, 60, 1) of a
    /// 64 by 6 buffer under `theme`, by a scene that reads nudges and is
    /// `live` or not.
    fn row_painted(parts: &[StatusPart], theme: &Theme, live: bool) -> (Buffer, ApcScene) {
        let input = [("ab".to_owned(), theme.get(scope::UI_TEXT))];
        let line = CmdLine {
            cwd: "~/p/stoat",
            input: &input,
            caret: None,
            status: CmdStatus::Parts(parts),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 64, 6));
        let mut scene = ApcScene::new();
        scene.set_live(live);
        scene.set_nudge_protocol(true);
        cmd_line_row(&mut buf, Rect::new(0, 3, 60, 1), &line, theme, &mut scene);
        (buf, scene)
    }

    /// A run of `text` at `col` and `row` sixteenths, in `color` over no
    /// background.
    fn bare_run(row: i16, col: i16, text: &str, color: [u8; 3]) -> Command {
        match run_on(row, col, text, color, [0; 3]) {
            Command::TextRun(run) => Command::TextRun(TextRunCommand { bg: None, ..run }),
            command => command,
        }
    }

    /// The parts end at the row's right edge 14 sixteenths apart, as runs over
    /// no background, and the cwd, the glyph, and the input stand in cells from
    /// the row's left.
    #[test]
    fn a_parts_status_ends_at_the_right_edge_with_its_gaps() {
        let parts = [
            part("\u{2713}", TEXT, None, false),
            part("18ms", MUTED, None, false),
        ];
        let (buf, scene) = row_painted(&parts, &Theme::empty(), true);

        assert_eq!(
            (commands(&scene), row(&buf, 3).trim_end().to_owned()),
            (
                vec![
                    bare_run(48, 866, "\u{2713}", TEXT),
                    bare_run(48, 896, "18ms", MUTED)
                ],
                format!("~/p/stoat {PROMPT_GLYPH} ab"),
            ),
        );
    }

    /// A part with `chip` set draws through the Source Tag painter, its box a
    /// rounded bar in the part's background under its label, and the part
    /// after it keeps the gap.
    #[test]
    fn a_tag_part_draws_through_the_source_tag_painter() {
        let parts = [
            part("cargo test", TEXT, Some(GROUND), true),
            part("\u{2713}", MUTED, None, false),
        ];
        let (_, scene) = row_painted(&parts, &Theme::empty(), true);

        let drawn = commands(&scene);
        let bars: Vec<_> = drawn
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => {
                    Some((bar.x, bar.y, bar.width, bar.height, bar.radius, bar.color))
                },
                _ => None,
            })
            .collect();
        let runs: Vec<_> = drawn
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text.as_str(), run.color)),
                _ => None,
            })
            .collect();
        assert_eq!(
            (bars, runs),
            (
                vec![(750, 49, 180, 14, 4, GROUND)],
                vec![(760, "cargo test", TEXT), (944, "\u{2713}", MUTED)],
            ),
        );
    }

    /// A scene that is not live writes each part's text in cells in its style,
    /// one blank cell apart, the last ending at the row's right edge.
    #[test]
    fn a_dead_scene_writes_the_parts_in_cells() {
        let parts = [
            part("cargo test", TEXT, Some(GROUND), true),
            part("\u{2713}", MUTED, None, false),
        ];
        let (buf, scene) = row_painted(&parts, &Theme::empty(), false);

        let cells: Vec<_> = (48..60)
            .map(|x| {
                let cell = &buf[(x, 3)];
                (cell.symbol().to_owned(), cell.fg, cell.bg)
            })
            .collect();
        let styled = |text: &str, style: Style| {
            let colors = (
                style.fg.unwrap_or(Color::Reset),
                style.bg.unwrap_or(Color::Reset),
            );
            text.chars()
                .map(move |ch| (ch.to_string(), colors.0, colors.1))
                .collect::<Vec<_>>()
        };
        let mut want = styled("cargo test", parts[0].style);
        want.extend(styled(" ", Style::default()));
        want.extend(styled("\u{2713}", parts[1].style));
        assert_eq!((cells, commands(&scene)), (want, Vec::new()));
    }

    /// A pane's row under the plate's theme stays in cells. It draws no nudge
    /// and no bar, rules no row under it, and leaves each cell's background
    /// unset.
    #[test]
    fn a_pane_row_carries_no_rule_and_no_plate_ground() {
        let parts = [part("\u{2713}", MUTED, None, false)];
        let (buf, scene) = row_painted(&parts, &plate_theme(), true);

        let chrome: Vec<_> = commands(&scene)
            .into_iter()
            .filter(|command| matches!(command, Command::Nudge(_) | Command::Bar(_)))
            .collect();
        let grounds: Vec<_> = (0..64)
            .map(|x| buf[(x, 3)].bg)
            .filter(|bg| *bg != Color::Reset)
            .collect();
        assert_eq!(
            (chrome, grounds, row(&buf, 4).trim().to_owned()),
            (Vec::new(), Vec::new(), String::new()),
        );
    }

    /// Under the plate a parts status ends at the row's right as runs over no
    /// background, inside the header's nudge.
    #[test]
    fn a_plate_header_ends_in_its_parts() {
        let theme = plate_theme();
        let input = [("ab".to_owned(), theme.get(scope::UI_TEXT))];
        let parts = [
            part("\u{2713}", TEXT, None, false),
            part("18ms", MUTED, None, false),
        ];
        let line = CmdLine {
            cwd: "~/p/stoat",
            input: &input,
            caret: Some(2),
            status: CmdStatus::Parts(&parts),
        };
        let (_, scene) = paint_line(&line, Rect::new(2, 1, 60, 4), &theme, true);

        assert_eq!(
            commands(&scene),
            [
                Command::Nudge([1, 7]),
                run(32, "~/p/stoat", CWD, GROUND),
                run(190, PROMPT_GLYPH, PROMPT, GROUND),
                run(220, "ab", TEXT, GROUND),
                run(252, " ", GROUND, TEXT),
                bare_run(16, 898, "\u{2713}", TEXT),
                bare_run(16, 928, "18ms", MUTED),
                Command::Nudge([0, 0]),
            ],
        );
    }

    #[test]
    fn token_spans_color_the_token_alone() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.text.fg = "#040404";
                ui.cmd.command = { fg: "#0e0e0e", modifiers: [bold] };
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let empty = Theme::empty();
        let spans = |token: Style, text: Style| {
            vec![("cargo".to_owned(), token), (" test".to_owned(), text)]
        };

        assert_eq!(
            [
                token_spans("cargo test", 5, &theme),
                token_spans("cargo test", 5, &empty)
            ],
            [
                spans(theme.get(scope::UI_CMD_COMMAND), theme.get(scope::UI_TEXT)),
                spans(empty.get(scope::UI_TEXT), empty.get(scope::UI_TEXT)),
            ],
        );
    }
}
