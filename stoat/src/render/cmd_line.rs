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
        chrome,
        paint::style_rgb,
        picker::{
            blank_plate_row, plate_run, plate_trailing, window_start, PlateTrailing,
            MIN_QUERY_COLUMNS, PLATE_HEADER_GAP, PLATE_HEADER_NUDGE,
        },
        text::{pre_wrap_rows, write_str, write_str_clipped},
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
use stoat_widgets::{text_run, ApcScene};
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

    let rows = paint_cell_rows(buf, inner, line, theme);
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
/// under the plate look where `plate` holds, from 1 to [`MAX_CMD_ROWS`].
///
/// The painter breaks the input by this same measure, so a caller that lays a
/// body out under the line reads the rows that the paint takes.
pub(crate) fn cmd_line_rows(line: &CmdLine<'_>, width: u16, plate: bool, theme: &Theme) -> u16 {
    let room = match plate {
        true => plate_measure(line, width, theme).room,
        false => cell_measure(line, width).room,
    };
    shown_rows(line, room).len() as u16
}

/// Where the parts of a cell row stand, in cells from the row's left.
struct CellMeasure {
    show_cwd: bool,
    /// The status's first cell, when the row keeps the status.
    status_col: Option<u16>,
    /// The input's first cell.
    input_col: u16,
    /// The chars of input that one row holds.
    room: usize,
}

/// Where the parts of a row `width` cells wide stand in the cell look.
///
/// The cwd, the glyph, and the input stand one blank cell apart. The status
/// ends flush with the row's right end, one blank cell clear of the input.
fn cell_measure(line: &CmdLine<'_>, width: u16) -> CellMeasure {
    let status_width: u16 = status_spans(&line.status)
        .iter()
        .map(|(text, _)| text.width() as u16)
        .sum();
    let cwd_width = line.cwd.width() as u16;
    let glyph_width = PROMPT_GLYPH.width() as u16;

    let lead = |cwd: bool| match cwd && cwd_width > 0 {
        true => cwd_width + 1 + glyph_width + 1,
        false => glyph_width + 1,
    };
    let fits = |cwd: bool, status: bool| {
        let trail = match status && status_width > 0 {
            true => status_width + 1,
            false => 0,
        };
        u32::from(lead(cwd)) + u32::from(MIN_QUERY_COLUMNS) + u32::from(trail) <= u32::from(width)
    };
    let (show_cwd, show_status) = parts_kept(fits);

    let input_col = lead(show_cwd).min(width);
    let status_col = (show_status && status_width > 0).then(|| width - status_width);
    let input_end = status_col.map_or(width, |col| col.saturating_sub(1));
    CellMeasure {
        show_cwd,
        status_col,
        input_col,
        room: usize::from(input_end.saturating_sub(input_col)),
    }
}

/// Where the parts of a plate row stand, in sixteenths from the row's left.
struct PlateMeasure {
    show_cwd: bool,
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
    let end = width.saturating_mul(16);

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
        let trail = match shown && !status.is_empty() {
            true => advance(&status, caption).saturating_add(PLATE_HEADER_GAP),
            false => 0,
        };
        u32::from(query_min(cwd)) + u32::from(trail) <= u32::from(end)
    };
    let (show_cwd, show_status) = parts_kept(fits);
    let shown = Some(status.as_str()).filter(|status| show_status && !status.is_empty());
    let trailing = plate_trailing(shown, &[], false, end, query_min(show_cwd), caption);

    let input_x = input_at(show_cwd);
    let room =
        usize::from(trailing.query_end.saturating_sub(input_x)) * 16 / usize::from(ui.max(1));
    PlateMeasure {
        show_cwd,
        glyph_x: glyph_at(show_cwd),
        input_x,
        trailing,
        room,
    }
}

/// Paint `line` into `inner`'s first rows as cells, for [`cmd_line`], and
/// answer how many rows the input takes.
fn paint_cell_rows(buf: &mut Buffer, inner: Rect, line: &CmdLine<'_>, theme: &Theme) -> u16 {
    let (y, end) = (inner.y, inner.right());
    let measure = cell_measure(line, inner.width);

    let mut x = inner.x;
    if measure.show_cwd && !line.cwd.is_empty() {
        write_str_clipped(buf, x, y, line.cwd, cwd_style(theme), end);
        x = x.saturating_add(line.cwd.width() as u16 + 1);
    }
    write_str_clipped(buf, x, y, PROMPT_GLYPH, prompt_style(theme), end);
    if let Some(col) = measure.status_col {
        let mut at = inner.x + col;
        for (text, style) in status_spans(&line.status) {
            write_str(buf, at, y, text, *style);
            at += text.width() as u16;
        }
    }

    let input_x = inner.x + measure.input_col;
    let input_end = input_x.saturating_add(u16::try_from(measure.room).unwrap_or(u16::MAX));
    let rows = shown_rows(line, measure.room);
    for (row_y, row) in (inner.y..inner.bottom()).zip(&rows) {
        let mut at = input_x;
        for piece in row_pieces(line, row.clone()) {
            let style = match piece.caret {
                true => theme.cursor_style(),
                false => piece.style,
            };
            write_str_clipped(buf, at, row_y, piece.text, style, input_end);
            at = at.saturating_add(piece.text.width() as u16);
        }
    }
    (rows.len() as u16).min(inner.height)
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

    scene.nudge(PLATE_HEADER_NUDGE);
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

/// The spans of `status`, empty for a row with none.
fn status_spans<'a>(status: &CmdStatus<'a>) -> &'a [(String, Style)] {
    match status {
        CmdStatus::None => &[],
        CmdStatus::Text(spans) => spans,
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
    use super::{cmd_line, cmd_line_rows, CmdLine, CmdStatus, PROMPT_GLYPH};
    use crate::{
        test_fixture,
        theme::{scope, Theme},
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
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
        let mut buf = Buffer::empty(Rect::new(0, 0, 64, 6));
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(nudges);
        with_line(parts, theme, |line| {
            cmd_line(&mut buf, inner, line, theme, &mut scene)
        });
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
        let rows = with_line(parts, &theme, |line| cmd_line_rows(line, 60, true, &theme));
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
        commands.extend([run(848, "14 of 890", MUTED, GROUND), Command::Nudge([0, 0])]);
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
                run(848, "14 of 890", MUTED, GROUND),
                Command::Nudge([0, 0]),
            ],
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
}
