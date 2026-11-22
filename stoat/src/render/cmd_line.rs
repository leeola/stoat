//! The Cmd Line, the one row that leads a prompt where the user types a
//! command.
//!
//! The row holds the directory the command runs in, the prompt glyph, the
//! input with its caret, and a status at the row's right. The command
//! palette's header is one. Every prompt that runs a command paints its row
//! here, so the rows read alike.

use crate::{
    render::{
        chrome,
        paint::style_rgb,
        picker::{
            blank_plate_row, plate_run, plate_trailing, query_window, MIN_QUERY_COLUMNS,
            PLATE_HEADER_GAP, PLATE_HEADER_NUDGE,
        },
        text::{write_str, write_str_clipped},
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

/// What a Cmd Line shows at its right end.
pub(crate) enum CmdStatus<'a> {
    /// No status, as over a free-typed argument, which lists nothing to count.
    None,
    /// Styled spans set flush right, such as the count of the list below.
    Text(&'a [(String, Style)]),
}

/// The parts of one Cmd Line row, for [`cmd_line`].
pub(crate) struct CmdLine<'a> {
    /// The directory the command runs in, already abbreviated, or empty for a
    /// row that names none.
    pub(crate) cwd: &'a str,
    /// What the user typed, as styled spans that read in order as one text.
    pub(crate) input: &'a [(String, Style)],
    /// The char index of the caret into the joined input, or `None` for a row
    /// that draws no caret.
    pub(crate) caret: Option<usize>,
    /// The words at the row's right end.
    pub(crate) status: CmdStatus<'a>,
}

/// Chars of the input in one style, as a row's window shows them.
struct Piece<'a> {
    text: &'a str,
    style: Style,
    /// Whether the piece is the caret's char, which draws in the caret's look.
    caret: bool,
}

/// Paint `line` into `inner`'s first row, and rule the second row off from the
/// body under it.
///
/// `inner` is a modal's rect inside its border, so a caller's body starts at
/// `inner.y + 2`. The cwd leads in `ui.cmd.cwd`, the [`PROMPT_GLYPH`] follows
/// in `ui.cmd.prompt`, and the input follows the glyph. The input shows the
/// chars that fit, with the caret in view, as [`query_window`] cuts them. The
/// status ends at the row's right. A row too narrow for the cwd, the glyph,
/// [`MIN_QUERY_COLUMNS`] of input, and the status drops the status first and
/// the cwd second, because the input is the part the user types in.
///
/// Under the plate look of [`plate_colors`](chrome::plate_colors), the row draws
/// as runs in a 32 px band on the plate's ground, [`PLATE_HEADER_NUDGE`] px off
/// its cells, and no rule draws under it. The parts stand [`PLATE_HEADER_GAP`]
/// apart at the [`TypeRole::Ui`] size, the status at the [`TypeRole::Caption`]
/// size, and the caret's char draws in the ground over the `ui.text` color.
/// Otherwise the parts draw in cells one blank cell apart, the caret's cell
/// takes [`Theme::cursor_style`], and a rule in `ui.border.inactive` fills the
/// second row. A part whose color has no RGB value draws no run.
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
        paint_plate_row(buf, inner, line, plate.ground, theme, scene);
        return;
    }

    paint_cell_row(buf, inner, line, theme);
    chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        theme.get(scope::UI_BORDER_INACTIVE),
        scene,
    );
}

/// Paint `line` into `inner`'s first row as cells, for [`cmd_line`].
///
/// The cwd, the glyph, and the input stand one blank cell apart. The status
/// ends flush with the row's right end, one blank cell clear of the input.
fn paint_cell_row(buf: &mut Buffer, inner: Rect, line: &CmdLine<'_>, theme: &Theme) {
    let (y, end) = (inner.y, inner.right());
    let spans = status_spans(&line.status);
    let status_width: u16 = spans.iter().map(|(text, _)| text.width() as u16).sum();
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
        u32::from(lead(cwd)) + u32::from(MIN_QUERY_COLUMNS) + u32::from(trail)
            <= u32::from(inner.width)
    };
    let (show_cwd, show_status) = parts_kept(fits);

    let mut x = inner.x;
    if show_cwd && cwd_width > 0 {
        write_str_clipped(buf, x, y, line.cwd, cwd_style(theme), end);
        x = x.saturating_add(cwd_width + 1);
    }
    write_str_clipped(buf, x, y, PROMPT_GLYPH, prompt_style(theme), end);
    let input_x = x.saturating_add(glyph_width + 1).min(end);

    let mut right = end;
    if show_status && status_width > 0 {
        right -= status_width;
        let mut at = right;
        for (text, style) in spans {
            write_str(buf, at, y, text, *style);
            at += text.width() as u16;
        }
        right = right.saturating_sub(1);
    }

    let room = usize::from(right.saturating_sub(input_x));
    let mut at = input_x;
    for piece in input_window(line, room) {
        let style = match piece.caret {
            true => theme.cursor_style(),
            false => piece.style,
        };
        write_str_clipped(buf, at, y, piece.text, style, right);
        at = at.saturating_add(piece.text.width() as u16);
    }
}

/// Paint `line` into `inner`'s first row as a plate's header, runs in the
/// boards' 32 px band on the plate's `ground`, for [`cmd_line`].
///
/// The cwd starts at the row's left, and each run of the input starts where
/// [`text_run::advance_sixteenths`] places its first char in the window. The
/// status ends at the row's right, as [`plate_trailing`] places a count.
fn paint_plate_row(
    buf: &mut Buffer,
    inner: Rect,
    line: &CmdLine<'_>,
    ground: [u8; 3],
    theme: &Theme,
    scene: &mut ApcScene,
) {
    blank_plate_row(buf, inner, ground);

    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let advance = |text: &str, scale| text_run::advance_sixteenths(text.chars().count(), scale);
    let spans = status_spans(&line.status);
    let status: String = spans.iter().map(|(text, _)| text.as_str()).collect();
    let end = inner.width.saturating_mul(16);

    let glyph_x = |cwd: bool| match cwd && !line.cwd.is_empty() {
        true => advance(line.cwd, ui).saturating_add(PLATE_HEADER_GAP),
        false => 0,
    };
    let input_x = |cwd: bool| {
        glyph_x(cwd)
            .saturating_add(advance(PROMPT_GLYPH, ui))
            .saturating_add(PLATE_HEADER_GAP)
    };
    let query_min = |cwd: bool| input_x(cwd).saturating_add(MIN_QUERY_COLUMNS * 16);
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

    let input_at = input_x(show_cwd);
    let room =
        usize::from(trailing.query_end.saturating_sub(input_at)) * 16 / usize::from(ui.max(1));
    let pieces = input_window(line, room);
    let text = style_rgb(theme.get(scope::UI_TEXT).fg);
    let bold = |style: Style| style.add_modifier.contains(Modifier::BOLD);

    scene.nudge(PLATE_HEADER_NUDGE);
    let origin = Rect::new(inner.x, inner.y, 1, 1);
    let mut run = |col: u16, part: &str, scale: u16, colors: (Option<[u8; 3]>, [u8; 3]), bold| {
        if let (Some(color), false) = (colors.0, part.is_empty()) {
            plate_run(col, part, scale, color, colors.1, bold).render(origin, buf, scene);
        }
    };

    if show_cwd {
        let cwd = cwd_style(theme);
        run(0, line.cwd, ui, (style_rgb(cwd.fg), ground), bold(cwd));
    }
    let prompt = prompt_style(theme);
    run(
        glyph_x(show_cwd),
        PROMPT_GLYPH,
        ui,
        (style_rgb(prompt.fg), ground),
        bold(prompt),
    );

    let mut index = 0;
    for piece in &pieces {
        let col = input_at.saturating_add(text_run::advance_sixteenths(index, ui));
        match (piece.caret, text) {
            (true, Some(text)) => run(col, piece.text, ui, (Some(ground), text), false),
            (true, None) => {},
            (false, _) => run(
                col,
                piece.text,
                ui,
                (style_rgb(piece.style.fg), ground),
                bold(piece.style),
            ),
        }
        index += piece.text.chars().count();
    }

    if let Some(mut col) = trailing.count {
        for (part, style) in spans {
            run(
                col,
                part,
                caption,
                (style_rgb(style.fg), ground),
                bold(*style),
            );
            col = col.saturating_add(advance(part, caption));
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

/// The pieces of `line`'s input that a row with `room` chars for it shows, as
/// [`query_window`] fits them around the caret.
///
/// Each piece holds chars of one style, and the caret's char stands alone. A
/// caret past the input's end covers a space. A line with no caret shows the
/// window that a caret at the input's end gives, and draws no caret.
fn input_window<'a>(line: &CmdLine<'a>, room: usize) -> Vec<Piece<'a>> {
    let joined: String = line.input.iter().map(|(text, _)| text.as_str()).collect();
    let len = joined.chars().count();
    let caret = line.caret.unwrap_or(len).min(len);
    let (before, at_caret, after) = query_window(&joined, caret, room);
    if at_caret.is_none() {
        return Vec::new();
    }

    let start = caret - before.chars().count();
    let stop = (caret + 1 + after.chars().count()).min(len);
    let shown_caret = line.caret.map(|_| caret);
    let mut pieces = spans_between(line.input, start..stop, shown_caret);
    if shown_caret == Some(len) {
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
    use super::{cmd_line, CmdLine, CmdStatus, PROMPT_GLYPH};
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

    /// Paint the line of `cwd`, the input `input` with its caret at `caret`,
    /// and the muted `status` into `inner` of a 64 by 6 buffer under `theme`,
    /// by a scene that reads nudges where `nudges` holds.
    fn painted(
        (cwd, input, caret, status): (&str, &str, Option<usize>, &str),
        inner: Rect,
        theme: &Theme,
        nudges: bool,
    ) -> (Buffer, ApcScene) {
        let input = [(input.to_owned(), theme.get(scope::UI_TEXT))];
        let spans = [(status.to_owned(), theme.get(scope::UI_TEXT_MUTED))];
        let line = CmdLine {
            cwd,
            input: &input,
            caret,
            status: match status.is_empty() {
                true => CmdStatus::None,
                false => CmdStatus::Text(&spans),
            },
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 64, 6));
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(nudges);
        cmd_line(&mut buf, inner, &line, theme, &mut scene);
        (buf, scene)
    }

    /// A run of `text` at `col` sixteenths on row 1, in `color` over `bg`.
    fn run(col: i16, text: &str, color: [u8; 3], bg: [u8; 3]) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row: 16,
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

    /// A 40-char input in a row with room for 10 chars shows its 9-char tail and
    /// the caret past its end, as `query_window` cuts the window.
    #[test]
    fn the_cmd_line_window_keeps_the_caret_in_view() {
        let input = "0123456789".repeat(4);
        let (buf, _) = painted(
            ("", &input, Some(40), ""),
            Rect::new(0, 0, 12, 2),
            &Theme::empty(),
            false,
        );

        let caret = buf[(11, 0)].modifier.contains(Modifier::REVERSED);
        assert_eq!(
            (&row(&buf, 0)[..], caret),
            (
                format!("\u{276f} 123456789 {}", " ".repeat(52)).as_str(),
                true
            ),
        );
    }
}
