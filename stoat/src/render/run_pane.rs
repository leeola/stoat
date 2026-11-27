use crate::{
    jobs,
    render::{
        chrome,
        cmd_line::{self, CmdLine, CmdStatus},
        paint::{palette_index, style_rgb},
        pixel_rows::{self, RowMetrics, ROW_PAD},
        text::{write_str, write_str_clipped},
        TypeScale,
    },
    run::{
        cargo::CmdReport,
        vterm::{self, BodyRow},
        BlockShape, Cmd, CmdId, GridSelection, RunBox, RunState, StyledCell, VtermGrid,
    },
    term_session::{TermId, TermSession},
    theme::TypeRole,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::{ops::Range, path::Path};
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
};
use unicode_width::UnicodeWidthChar;

/// The note at the right of a Run pane prompt that shows a line a walk of the
/// command history recalled, which names the keys and the rule of the walk.
pub(crate) const RECALL_NOTE: &str = "\u{2191} history \u{b7} keyword match";

/// One part of the status that ends a Run Block's header, such as its mark or
/// its runtime.
pub(crate) struct StatusPart {
    pub(crate) text: String,
    pub(crate) style: Style,
    /// Whether the part draws as a tag, its text in a box, rather than as
    /// words.
    pub(crate) chip: bool,
}

/// What a pixel stack reads besides the run itself, for [`paint_pixel_stack`].
#[derive(Clone, Copy)]
struct StackSources<'a> {
    cmds: &'a slotmap::SlotMap<CmdId, Cmd>,
    terms: &'a slotmap::SlotMap<TermId, TermSession>,
    editors: &'a slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
    theme: &'a crate::theme::Theme,
    /// The workspace root, which the prompt names as its cwd.
    root: &'a Path,
    home: Option<&'a Path>,
}

/// The owned text of a Cmd Line that a Run pane draws, a block's header or the
/// prompt, which [`Self::line`] lends as a [`CmdLine`].
struct LineParts {
    cwd: String,
    input: Vec<(String, Style)>,
    caret: Option<usize>,
    parts: Vec<StatusPart>,
}

impl LineParts {
    /// The header of `block`, whose line ran `command` in `cwd`.
    fn header(
        block: &Cmd,
        (cwd, command): (&Path, &str),
        home: Option<&Path>,
        terms: &slotmap::SlotMap<TermId, TermSession>,
        theme: &crate::theme::Theme,
    ) -> LineParts {
        let token = command.chars().take_while(|ch| !ch.is_whitespace()).count();
        LineParts {
            cwd: crate::run::abbreviate_path(cwd, home),
            input: cmd_line::token_spans(command, token, theme),
            caret: None,
            parts: status_parts(block, terms, theme),
        }
    }

    /// The prompt of `run_state`, with the workspace `root` as its cwd and its
    /// caret while the pane holds the focus.
    ///
    /// The command token and the tag at the right are the ones that
    /// [`RunState::prompt`] holds. A line that a walk of the command history
    /// recalled ends in [`RECALL_NOTE`] in `ui.text.muted`.
    fn prompt(
        run_state: &RunState,
        (root, home): (&Path, Option<&Path>),
        editors: &slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
        theme: &crate::theme::Theme,
        is_focused: bool,
    ) -> LineParts {
        let (text, caret) = run_state.input.caret_in(editors);
        let mut parts: Vec<StatusPart> =
            lead_part(run_state.prompt.chip, run_state.prompt.source, theme)
                .into_iter()
                .collect();
        if run_state.prompt.recalled {
            parts.push(StatusPart {
                text: RECALL_NOTE.to_owned(),
                style: theme.get(crate::theme::scope::UI_TEXT_MUTED),
                chip: false,
            });
        }
        LineParts {
            cwd: crate::run::abbreviate_path(root, home),
            input: cmd_line::token_spans(&text, run_state.prompt.token_len, theme),
            caret: is_focused.then_some(caret),
            parts,
        }
    }

    /// The line these parts read as, with no status where they hold no part.
    fn line(&self) -> CmdLine<'_> {
        CmdLine {
            cwd: &self.cwd,
            input: &self.input,
            caret: self.caret,
            status: match self.parts.is_empty() {
                true => CmdStatus::None,
                false => CmdStatus::Parts(&self.parts),
            },
        }
    }
}

/// The nudge and the ink that a pixel stack last sent, so a line sends each
/// only where it differs from the one in force.
pub(super) struct Pen {
    /// The content's left in logical px, which every line is nudged right by.
    dx: i16,
    nudge: Option<[i16; 2]>,
    ink: Option<u8>,
}

impl Pen {
    fn new(b: &RunBox) -> Pen {
        Pen::at((b.left as f32 / b.metrics.scale).round() as i16)
    }

    /// A pen whose every line is nudged `dx` logical px right, with no nudge
    /// and no ink sent yet.
    pub(super) fn at(dx: i16) -> Pen {
        Pen {
            dx,
            nudge: None,
            ink: None,
        }
    }

    /// Put in force the nudge that sets a line `dy` logical px down of its row.
    pub(super) fn nudge(&mut self, dy: i16, scene: &mut stoat_widgets::ApcScene) {
        let nudge = [self.dx, dy];
        if self.nudge != Some(nudge) {
            scene.nudge(nudge);
            self.nudge = Some(nudge);
        }
    }

    /// The cell row of `b`'s area that holds the physical px row `y`, after the
    /// nudge that sets a line's top on `y` and its left on the content's.
    fn line(&mut self, b: &RunBox, y: u32, scene: &mut stoat_widgets::ApcScene) -> Rect {
        let (row, dy) = b.metrics.place(y);
        self.nudge(dy, scene);
        Rect::new(b.area.x, b.area.y.saturating_add(row), b.area.width, 1)
    }

    /// Put `ink` in force for the runs and the bars after it.
    pub(super) fn ink(&mut self, ink: Option<u8>, scene: &mut stoat_widgets::ApcScene) {
        if self.ink != ink {
            scene.ink(ink);
            self.ink = ink;
        }
    }

    /// Draw a bar in `color` from the content's left, its top on the pixel row
    /// `y`, `size` sixteenths wide and high.
    fn bar(
        &mut self,
        b: &RunBox,
        y: u32,
        [width, height]: [u16; 2],
        color: [u8; 3],
        buf: &mut Buffer,
        scene: &mut stoat_widgets::ApcScene,
    ) {
        self.ink(None, scene);
        let row = self.line(b, y, scene);
        Bar {
            x: 0,
            y: 0,
            width,
            height,
            color,
            radius: 0,
            corners: 0,
        }
        .render(row, buf, scene);
    }
}

/// How a color of an output cell draws at the board's pixels.
#[derive(Clone, Copy, PartialEq)]
enum CellInk {
    /// An RGB color, which the run or the bar states.
    Rgb([u8; 3]),
    /// An entry of the terminal's palette, which an ink names.
    Palette(u8),
}

impl CellInk {
    /// The ink of the cell color `color`, or `fallback` for a cell with none.
    fn of(color: Option<Color>, fallback: [u8; 3]) -> CellInk {
        match color {
            Some(Color::Rgb(r, g, b)) => CellInk::Rgb([r, g, b]),
            Some(color) => palette_index(color).map_or(CellInk::Rgb(fallback), CellInk::Palette),
            None => CellInk::Rgb(fallback),
        }
    }

    /// The ink of the cell background `color`, or `None` for a cell with none.
    fn under(color: Option<Color>) -> Option<CellInk> {
        match color? {
            Color::Rgb(r, g, b) => Some(CellInk::Rgb([r, g, b])),
            color => palette_index(color).map(CellInk::Palette),
        }
    }

    /// Put this ink in force with `pen`, and answer the RGB that a run or a bar
    /// in it states, which is `placeholder` for a palette entry, since the ink
    /// draws over it.
    fn apply(
        self,
        placeholder: [u8; 3],
        pen: &mut Pen,
        scene: &mut stoat_widgets::ApcScene,
    ) -> [u8; 3] {
        match self {
            CellInk::Rgb(rgb) => {
                pen.ink(None, scene);
                rgb
            },
            CellInk::Palette(index) => {
                pen.ink(Some(index), scene);
                placeholder
            },
        }
    }
}

/// The colors that a grid row draws in where its cells name none.
#[derive(Clone, Copy)]
pub(super) struct CellColors {
    /// The color of output that the program left uncolored, `ui.text.dim`.
    pub(super) dim: [u8; 3],
    /// The Island's surface, which the text of a selected cell draws in.
    pub(super) surface: [u8; 3],
}

/// Where the runs and the bars of a row of output draw.
pub(super) struct RowAt {
    /// The one-row rect the runs anchor on.
    pub(super) anchor: Rect,
    /// The sixteenths from the anchor's left to where the row's first char
    /// draws.
    pub(super) col: u16,
    /// The text scale, which sets how far each char advances.
    pub(super) scale: u16,
    /// The height of a bar under a run, in sixteenths of a cell.
    pub(super) height: u16,
}

impl RowAt {
    /// A row of output on `anchor` from `col` sixteenths at `scale`, whose bar
    /// under a run is one body line high, the cell height of `m` less 1 px.
    pub(super) fn body(anchor: Rect, col: u16, scale: u16, m: &RowMetrics) -> RowAt {
        RowAt {
            anchor,
            col,
            scale,
            height: high16(m, u32::from(m.cell[1]).saturating_sub(m.px(1))),
        }
    }
}

/// Paint a Run pane's stack and its prompt row into `area`.
///
/// The prompt row takes the pane's last row, and the stack sits on it, so a
/// short stack leaves its blank rows at the top. Both stand one column inside
/// each side of the pane, as [`crate::run::stack_rect`] gives them, and a
/// block's body rows start one column inside its header. Output that the
/// program left uncolored draws in `ui.text.dim`.
///
/// A block that shows its report draws the report's rows as its body, as
/// [`Cmd::report_rows`] gives them, and the newest block's
/// [`RunState::hint`] row follows its last row.
///
/// The prompt row paints as a Cmd Line, as [`cmd_line::cmd_line_row`] paints
/// one, with the workspace root as its cwd. Its command token and the tag at
/// its right are the ones that [`RunState::prompt`] holds, and its caret shows
/// while the pane holds the focus. A line that a walk of the command history
/// recalled ends the row with [`RECALL_NOTE`] in `ui.text.muted`.
///
/// Inside stoatty a rule ends each block, a bar runs beside the body of a
/// failed block and of a Stub, and a rule tops the prompt row, as
/// [`paint_stack_rules`] draws them. A plain terminal draws none of them.
///
/// `terms` holds the sessions of the workspace, which tell whether a Stub's
/// tab still runs.
///
/// With `pixels`, the metrics of a host that draws nudged runs in the colors
/// of its palette and the reach of the pane's Island, the stack draws at the
/// board's pixels instead, as [`paint_pixel_stack`] lays it out. The measure it
/// drew with stays on [`RunState::pixel_box`], which a cell paint clears.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_run_pane(
    run_state: &mut RunState,
    cmds: &slotmap::SlotMap<CmdId, Cmd>,
    terms: &slotmap::SlotMap<TermId, TermSession>,
    editors: &slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
    theme: &crate::theme::Theme,
    root: &Path,
    home: Option<&Path>,
    area: Rect,
    is_focused: bool,
    pixels: Option<(RowMetrics, [i32; 4])>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    run_state.pixel_box = None;
    if area.height < 2 || area.width < 4 {
        return;
    }
    if let Some((metrics, reach)) = pixels {
        let b = RunBox::of(metrics, theme.type_scale(TypeRole::Ui), area, reach);
        run_state.pixel_box = Some(b);
        let sources = StackSources {
            cmds,
            terms,
            editors,
            theme,
            root,
            home,
        };
        paint_pixel_stack(run_state, &b, &sources, is_focused, buf, scene);
        return;
    }

    let inner = crate::run::stack_rect(area);
    let input_row = area.y + area.height - 1;
    let visible = usize::from(area.height - 1);

    let total = run_state.output_line_total(cmds);
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    let window = start..start + visible;
    let pad = visible.saturating_sub(total);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);

    // Walked block by block rather than flattened first. A block's grid holds
    // its whole scrollback, so materializing the rows would cost the history
    // on every paint to draw a pane's worth of it.
    let mut rule_rows = Vec::new();
    let mut body_bars = Vec::new();
    let mut base = 0;
    let hint = run_state.hint(cmds, terms);
    let newest = run_state.blocks.len().saturating_sub(1);
    let blocks = run_state
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, id)| cmds.get(*id).map(|block| (index, block)));
    for (index, block) in blocks {
        let report = block.report_rows();
        let content = match &report {
            Some(rows) => Body::Report(rows),
            None => Body::Grid(block.grid.rendered_line_count()),
        };
        let hint = hint.as_ref().filter(|_| index == newest);
        let span = block.rendered_line_span() + usize::from(hint.is_some());
        let offsets = window_offsets(base, span, &window);
        let row_of = |offset: usize| area.y + (pad + base + offset - start) as u16;

        for offset in offsets.clone() {
            let y = row_of(offset);
            let Some(line) = block_line(block, content, hint, offset) else {
                continue;
            };
            let body = Rect::new(inner.x + 1, y, inner.width.saturating_sub(1), 1);
            match &line {
                OutputLine::Prompt { cwd, command } => {
                    let header = LineParts::header(block, (cwd, command), home, terms, theme);
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    cmd_line::cmd_line_row(buf, row, &header.line(), theme, scene);
                },
                OutputLine::GridRow(grid, row_idx, selection) => {
                    paint_grid_row(buf, body, y, grid, *row_idx, *selection, dim);
                },
                OutputLine::Row(row) => paint_body_row(buf, body, row, theme),
                OutputLine::Error(msg) => {
                    let display: String = msg.chars().take(usize::from(body.width)).collect();
                    write_str(
                        buf,
                        body.x,
                        y,
                        &display,
                        theme.get(crate::theme::scope::UI_ERROR),
                    );
                },
            }
        }

        if !block.in_jobs
            && let Some((done, total)) = block.progress()
            && offsets.contains(&1)
        {
            let row = Rect::new(inner.x + 1, row_of(1), inner.width.saturating_sub(1), 1);
            let width = row.width.saturating_mul(16);
            paint_progress(row, 0, width, done, total, theme, buf, scene);
        }
        if offsets.contains(&(span - 1)) {
            rule_rows.push(row_of(span - 1));
        }
        let body = offsets.start.max(1)..offsets.end;
        if let Some(style) = body_bar_style(block, theme)
            && !body.is_empty()
        {
            body_bars.push((row_of(body.start), body.len() as u16, style));
        }
        base += span;
    }

    let prompt = LineParts::prompt(run_state, (root, home), editors, theme, is_focused);
    let row = Rect::new(inner.x, input_row, inner.width, 1);
    cmd_line::cmd_line_row(buf, row, &prompt.line(), theme, scene);

    // The plain terminal has no room for a rule between the rows, so the
    // stack and the prompt stay flush there.
    if scene.live() {
        paint_stack_rules(buf, inner, &rule_rows, &body_bars, input_row, theme, scene);
    }
}

/// Paint `run_state`'s stack and its prompt as runs and bars at the board's
/// pixels, as `b` measures them, for [`render_run_pane`].
///
/// Each line on show, as [`RunState::window_px`] places it, draws under the
/// nudge that sets its text top on its pixel row and its left on the content's
/// left, and no cell is written. A header and the prompt draw as
/// [`cmd_line::cmd_line_runs`] draws a Cmd Line, and a grid row as
/// [`paint_pixel_grid_row`] draws one, 10 px in. A report row, a Stub's row,
/// and the error line draw as spans 10 px in, and the hint does too at the
/// [`TypeRole::Caption`] size.
///
/// After the lines come the bar beside each body that carries one, 2 px wide at
/// the content's left from the top of its first line on show to the bottom of
/// its last, the rule of each block whose last line is on show, 4 px under that
/// line, and the rule of the prompt. The progress bar of a running Cmd comes
/// last, on the top of its first body line from 10 px in to the content's
/// right. The paint ends with no ink and no nudge in force.
fn paint_pixel_stack(
    run_state: &RunState,
    b: &RunBox,
    sources: &StackSources<'_>,
    is_focused: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let StackSources {
        cmds,
        terms,
        editors,
        theme,
        root,
        home,
    } = *sources;
    let m = b.metrics;
    let cell_width = u32::from(m.cell[0].max(1));
    let width16 = u16::try_from((b.width * 16 + cell_width / 2) / cell_width).unwrap_or(u16::MAX);
    let (gap16, body16) = (m.cols(6), m.cols(10));
    let caption = theme.type_scale(TypeRole::Caption);
    let dim = style_rgb(theme.get(crate::theme::scope::UI_TEXT_DIM).fg).unwrap_or_default();
    let colors = CellColors {
        dim,
        surface: chrome::plate_colors(theme, scene).map_or(dim, |plate| plate.surface),
    };
    let mut pen = Pen::new(b);

    let hint = run_state.hint(cmds, terms);
    let (mut bodies, mut rules, mut progress) = (Vec::new(), Vec::new(), Vec::new());
    let lines = run_state.window_px(cmds, b);
    for group in lines.chunk_by(|one, next| one.cmd == next.cmd) {
        let (first, last) = (group[0], group[group.len() - 1]);
        let Some(block) = cmds.get(first.cmd) else {
            continue;
        };
        let hint = hint
            .as_ref()
            .filter(|_| run_state.blocks.last() == Some(&first.cmd));
        let shape = BlockShape::of(block, hint.is_some());
        let report = block.report_rows();
        let content = match &report {
            Some(rows) => Body::Report(rows),
            None => Body::Grid(block.grid.rendered_line_count()),
        };
        let top = i64::from(first.top)
            - i64::from(shape.slot_top(b, first.line) + shape.text_inset(b, first.line));
        let slot = |line: usize| {
            let start = top + i64::from(shape.slot_top(b, line));
            (start, start + i64::from(shape.slot_height(b, line)))
        };

        for line in group {
            let Some(out) = block_line(block, content, hint, line.line) else {
                continue;
            };
            let row = pen.line(b, line.top, scene);
            match out {
                OutputLine::Prompt { cwd, command } => {
                    pen.ink(None, scene);
                    let header = LineParts::header(block, (cwd, command), home, terms, theme);
                    cmd_line::cmd_line_runs(buf, row, width16, gap16, &header.line(), theme, scene);
                },
                OutputLine::GridRow(grid, row_idx, selection) => {
                    let at = RowAt::body(row, body16, b.scale, &m);
                    paint_pixel_grid_row(
                        grid, row_idx, selection, &at, colors, &mut pen, buf, scene,
                    );
                },
                OutputLine::Row(body_row) => {
                    let scale = match shape.is_hint(line.line) {
                        true => caption,
                        false => b.scale,
                    };
                    let at = RowAt::body(row, body16, scale, &m);
                    paint_pixel_body_row(body_row, &at, theme, dim, &mut pen, buf, scene);
                },
                OutputLine::Error(msg) => {
                    pen.ink(None, scene);
                    let error = theme.get(crate::theme::scope::UI_ERROR);
                    pixel_rows::emit_spans(&[(msg, error)], body16, b.scale, row, dim, buf, scene);
                },
            }
        }

        if last.last {
            rules.push(slot(last.line).1 + i64::from(m.px(4)));
        }
        let bar = body_bar_style(block, theme).and_then(|style| style_rgb(style.fg));
        if let (Some(body), Some(color)) = (group.iter().find(|line| line.line > 0), bar) {
            bodies.push((slot(body.line).0, slot(last.line).1, color));
        }
        if !block.in_jobs
            && let Some((done, total)) = block.progress()
            && let Some(body) = group.iter().find(|line| line.line == 1)
        {
            progress.push((body.top, done, total));
        }
    }

    let prompt = LineParts::prompt(run_state, (root, home), editors, theme, is_focused);
    let row = pen.line(b, b.rule_y() + m.px(1) + m.px(6) + m.px(ROW_PAD), scene);
    pen.ink(None, scene);
    cmd_line::cmd_line_runs(buf, row, width16, gap16, &prompt.line(), theme, scene);

    let px = |y: i64| u32::try_from(y.max(0)).unwrap_or(u32::MAX);
    for (top, bottom, color) in bodies {
        let height = high16(&m, px(bottom).saturating_sub(px(top)));
        pen.bar(b, px(top), [m.cols(2), height], color, buf, scene);
    }
    let rule = theme
        .try_get(crate::theme::scope::UI_RUN_RULE)
        .and_then(|style| style_rgb(style.fg));
    for y in rules.into_iter().filter(|_| rule.is_some()) {
        let color = rule.unwrap_or_default();
        pen.bar(b, px(y), [width16, 1], color, buf, scene);
    }
    let prompt_rule = theme
        .try_get(crate::theme::scope::UI_RUN_PROMPT_RULE)
        .unwrap_or_else(|| theme.get(crate::theme::scope::UI_DIVIDER));
    if let Some(color) = style_rgb(prompt_rule.fg) {
        pen.bar(b, b.rule_y(), [width16, 1], color, buf, scene);
    }
    pen.ink(None, scene);
    for (top, done, total) in progress {
        let row = pen.line(b, top, scene);
        let width = width16.saturating_sub(body16);
        paint_progress(row, body16, width, done, total, theme, buf, scene);
    }

    scene.ink(None);
    scene.nudge([0, 0]);
}

/// Draw row `row_idx` of `grid` as runs and bars where `at` places them.
///
/// Each longest stretch of cells with one color, weight, and slant draws as one
/// run with no background, and each stretch of cells with one background, or
/// in the selection, as a bar under its runs. A color draws as the RGB it names
/// or under an ink of the palette entry it names, and a cell with no color
/// draws in `colors.dim`. A selected cell draws its bar in its text color and
/// its run in `colors.surface`, as a reversed cell draws in cells.
///
/// A row holds only the cells written to it, and a selection also covers the
/// columns past them. A cell advances as one char at `at.scale`, so a row's
/// runs stand on its grid columns. A wide char ends its run, so the blank cell
/// after it, its covered half, starts no run of its own.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_pixel_grid_row(
    grid: &VtermGrid,
    row_idx: usize,
    selection: Option<&GridSelection>,
    at: &RowAt,
    colors: CellColors,
    pen: &mut Pen,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let row = grid.row(row_idx);
    let width = usize::from(grid.width());
    let cols = match selection {
        Some(_) => width,
        None => row.len().min(width),
    };
    let unwritten = StyledCell::default();
    let cell = |col: usize| row.get(col).unwrap_or(&unwritten);
    let row_u16 = u16::try_from(row_idx).unwrap_or(u16::MAX);
    let selected = |col: usize| {
        let col = u16::try_from(col).unwrap_or(u16::MAX);
        selection.is_some_and(|sel| sel.contains(col, row_u16))
    };
    let x16 = |col: usize| {
        let x = at
            .col
            .saturating_add(text_run::advance_sixteenths(col, at.scale));
        i16::try_from(x).unwrap_or(i16::MAX)
    };

    let bar_of = |col: usize| match selected(col) {
        true => Some(CellInk::of(cell(col).fg, colors.dim)),
        false => CellInk::under(cell(col).bg),
    };
    let mut col = 0;
    while col < cols {
        let Some(ink) = bar_of(col) else {
            col += 1;
            continue;
        };
        let start = col;
        while col < cols && bar_of(col) == Some(ink) {
            col += 1;
        }
        let color = ink.apply(colors.dim, pen, scene);
        Bar {
            x: x16(start),
            y: 0,
            width: x16(col).abs_diff(x16(start)),
            height: at.height,
            color,
            radius: 0,
            corners: 0,
        }
        .render(at.anchor, buf, scene);
    }

    let face = |col: usize| {
        let modifiers = cell(col).modifiers;
        let text = match selected(col) {
            true => CellInk::Rgb(colors.surface),
            false => CellInk::of(cell(col).fg, colors.dim),
        };
        let bold = modifiers.contains(Modifier::BOLD);
        (text, bold, modifiers.contains(Modifier::ITALIC))
    };
    let mut text = String::new();
    let mut col = 0;
    while col < cols {
        let style = face(col);
        let mut first = None;
        let mut kept = 0;
        text.clear();
        while col < cols && face(col) == style {
            let ch = cell(col).ch;
            col += 1;
            if ch == ' ' && first.is_none() {
                continue;
            }
            first.get_or_insert(col - 1);
            text.push(ch);
            if ch != ' ' {
                kept = text.len();
            }
            if UnicodeWidthChar::width(ch) == Some(2) {
                break;
            }
        }
        text.truncate(kept);
        let Some(first) = first.filter(|_| !text.is_empty()) else {
            continue;
        };

        let (ink, bold, italic) = style;
        let color = ink.apply(colors.dim, pen, scene);
        TextRun {
            col: x16(first),
            row: 0,
            scale: at.scale,
            color,
            bg: None,
            bold,
            italic,
            follow: 0,
            anchor: None,
            text: &text,
        }
        .render(at.anchor, buf, scene);
    }
}

/// Draw `row`, a row of a report, a Stub's row, or a hint, as spans where `at`
/// places them, with no ink in force.
///
/// Each span draws in the foreground of its tone's scope, as
/// [`paint_body_row`] draws it in cells, and a scope with no RGB foreground
/// draws in `fallback`.
pub(super) fn paint_pixel_body_row(
    row: &BodyRow,
    at: &RowAt,
    theme: &crate::theme::Theme,
    fallback: [u8; 3],
    pen: &mut Pen,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let spans: Vec<(&str, Style)> = row
        .spans
        .iter()
        .map(|(text, tone)| {
            let fg = theme.get(tone.scope()).fg;
            (text.as_str(), Style::default().fg(fg.unwrap_or_default()))
        })
        .collect();
    pen.ink(None, scene);
    pixel_rows::emit_spans(&spans, at.col, at.scale, at.anchor, fallback, buf, scene);
}

/// The style of the bar beside `block`'s body, or `None` for a block that
/// carries none.
///
/// The Stub of a Job takes the color of the Job's state, as
/// [`jobs::state_style`] gives it, a Stub takes `ui.statusline.run`, and a
/// failed block takes `ui.error`.
fn body_bar_style(block: &Cmd, theme: &crate::theme::Theme) -> Option<Style> {
    if block.in_jobs {
        return Some(jobs::state_style(theme, jobs::state_of(block)));
    }
    if block.handoff.is_some() {
        return Some(theme.get(crate::theme::scope::UI_STATUSLINE_RUN));
    }
    (block.finished && block.exit_status != Some(0))
        .then(|| theme.get(crate::theme::scope::UI_ERROR))
}

/// The sixteenths of `m`'s cell height nearest `px` physical px.
fn high16(m: &RowMetrics, px: u32) -> u16 {
    let height = u32::from(m.cell[1].max(1));
    u16::try_from((px * 16 + height / 2) / height).unwrap_or(u16::MAX)
}

/// Draw the bar beside each body that carries one, the rule under each block,
/// and the rule over the prompt, across `inner`.
///
/// `rule_rows` holds the last row of each block on show, `body_bars` the
/// first row, the row count, and the style of each body bar on show, and
/// `prompt_row` the prompt's row.
///
/// A block's rule runs along its last row's bottom edge in `ui.run.rule`, and
/// a theme that leaves that unset draws none. A body bar is 4 sixteenths wide
/// at `inner`'s left edge in the foreground of its style: `ui.error` beside a
/// failed block's body, `ui.statusline.run` beside a Stub's, and the state
/// color of a Job beside the Stub of a Job. The prompt's rule runs along the
/// prompt row's top edge in `ui.run.prompt.rule`, or in `ui.divider` for a
/// theme that leaves that unset.
fn paint_stack_rules(
    buf: &mut Buffer,
    inner: Rect,
    rule_rows: &[u16],
    body_bars: &[(u16, u16, Style)],
    prompt_row: u16,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let width = inner.width.saturating_mul(16);
    let bar = |y: i16, width: u16, height: u16, color: [u8; 3]| Bar {
        x: 0,
        y,
        width,
        height,
        color,
        radius: 0,
        corners: 0,
    };

    for &(row, count, style) in body_bars {
        let Some(color) = style_rgb(style.fg) else {
            continue;
        };
        let body = Rect::new(inner.x, row, inner.width, count);
        bar(0, 4, count.saturating_mul(16), color).render(body, buf, scene);
    }

    // The rules go after the bars, so a block's rule runs unbroken under the
    // bar beside its body, as the board's border does.
    let rule = theme
        .try_get(crate::theme::scope::UI_RUN_RULE)
        .and_then(|style| style_rgb(style.fg));
    if let Some(color) = rule {
        for &row in rule_rows {
            bar(15, width, 1, color).render(Rect::new(inner.x, row, inner.width, 1), buf, scene);
        }
    }

    let prompt = theme
        .try_get(crate::theme::scope::UI_RUN_PROMPT_RULE)
        .unwrap_or_else(|| theme.get(crate::theme::scope::UI_DIVIDER));
    if let Some(color) = style_rgb(prompt.fg) {
        let row = Rect::new(inner.x, prompt_row, inner.width, 1);
        bar(0, width, 1, color).render(row, buf, scene);
    }
}

/// Draw a running Cmd's progress bar on the top two pixel rows of
/// `first_body_row`, the first row of its body, where no glyph reaches.
///
/// The track starts `x` sixteenths into the row and is `width` sixteenths wide,
/// in the `ui.divider` foreground, the color of a meter's track. The fill covers
/// `done` of `total` of it in the `ui.statusline.run` foreground. A plain
/// terminal draws neither.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_progress(
    first_body_row: Rect,
    x: u16,
    width: u16,
    done: u32,
    total: u32,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if !scene.live() || total == 0 {
        return;
    }
    let fill = u16::try_from(u32::from(width) * done.min(total) / total).unwrap_or(width);
    let bars = [
        (width, crate::theme::scope::UI_DIVIDER),
        (fill, crate::theme::scope::UI_STATUSLINE_RUN),
    ];
    for (width, scope) in bars {
        let Some(color) = style_rgb(theme.get(scope).fg).filter(|_| width > 0) else {
            continue;
        };
        Bar {
            x: i16::try_from(x).unwrap_or(i16::MAX),
            y: 0,
            width,
            height: 2,
            color,
            radius: 0,
            corners: 0,
        }
        .render(first_body_row, buf, scene);
    }
}

/// The status that ends `cmd`'s header, as the parts it reads in order.
///
/// A Parsed Cmd's status leads with its Kind Chip in `ui.run.kind`, or in
/// `ui.key_label` for a theme that leaves that unset, which draws the label
/// with no box. A Cmd with no Kind Chip that a config command started leads
/// with that command's Source Tag in the tag's scope, when the theme sets the
/// scope. The other parts follow the lead.
///
/// A finished Cmd reads its mark, U+2713 in `ui.badge.complete` for an exit of
/// 0, and otherwise U+2717 in `ui.error`, with `exit N` after it when the
/// process gave a code. Its runtime follows in `ui.text.muted`, with the run
/// count after it from the second run on. A Cmd with matches reads their
/// count and U+00B7 before its runtime, such as `3 matches`. A Cmd without
/// both a start and an end reads its mark alone. A `cargo test` Cmd whose
/// report counts a failed test reads U+2717, that count, and `failed` as its
/// mark, and a build whose report counts an error reads U+2717 and its count
/// of errors, such as `1 error`.
///
/// A running Cmd reads U+25CF and the whole seconds it has run, as
/// [`Cmd::elapsed_secs`] holds them, in the foreground of
/// `ui.statusline.run`. A running Cmd whose report counts its progress reads
/// the done count over the total between the two, such as `2 / 4`, as
/// [`Cmd::progress`] gives them. The status-line scope can carry a
/// background, which a header leaves to the pane.
///
/// A Stub reads one part. It is U+25CF and `Terminal tab`, or `Agent tab` for
/// an agent's, in the foreground of `ui.statusline.run` while `terms` holds
/// the session of its tab, and `tab closed` in `ui.text.muted` once the
/// workspace dropped the session. An agent's session stays until its tab
/// closes.
///
/// The Stub of a Job, a Cmd [`Cmd::in_jobs`], reads its lead and one part in
/// the color of its state, as [`jobs::state_style`] gives it. The part is the
/// state's mark and `in Jobs`, then U+00B7 and the progress counts of a
/// running Cmd, or else the time that [`jobs::time_text`] reads.
pub(crate) fn status_parts(
    cmd: &Cmd,
    terms: &slotmap::SlotMap<TermId, TermSession>,
    theme: &crate::theme::Theme,
) -> Vec<StatusPart> {
    if cmd.in_jobs {
        let state = jobs::state_of(cmd);
        let rest = match cmd.progress() {
            Some((done, total)) => format!("{done} / {total}"),
            None => jobs::time_text(cmd),
        };
        let mut parts: Vec<StatusPart> = lead_part(cmd.kind.chip(), cmd.source, theme)
            .into_iter()
            .collect();
        parts.push(StatusPart {
            text: format!("{} in Jobs \u{b7} {rest}", state.mark()),
            style: jobs::state_style(theme, state),
            chip: false,
        });
        return parts;
    }
    if let Some(handoff) = &cmd.handoff {
        let (text, style) = match terms.contains_key(handoff.term) {
            true => (
                match handoff.agent {
                    true => "\u{25cf} Agent tab",
                    false => "\u{25cf} Terminal tab",
                },
                Style {
                    fg: theme.get(crate::theme::scope::UI_STATUSLINE_RUN).fg,
                    ..Style::default()
                },
            ),
            false => ("tab closed", theme.get(crate::theme::scope::UI_TEXT_MUTED)),
        };
        return vec![StatusPart {
            text: text.to_owned(),
            style,
            chip: false,
        }];
    }

    let mut parts: Vec<StatusPart> = lead_part(cmd.kind.chip(), cmd.source, theme)
        .into_iter()
        .collect();

    if !cmd.finished {
        let fg = theme.get(crate::theme::scope::UI_STATUSLINE_RUN).fg;
        let elapsed = crate::run::format_elapsed(cmd.elapsed_secs);
        let text = match cmd.progress() {
            Some((done, total)) => format!("\u{25cf} {done} / {total} \u{b7} {elapsed}"),
            None => format!("\u{25cf} {elapsed}"),
        };
        parts.push(StatusPart {
            text,
            style: Style {
                fg,
                ..Style::default()
            },
            chip: false,
        });
        return parts;
    }

    let failed = match &cmd.report {
        Some(CmdReport::Test(report)) => {
            let failed = report.totals().1;
            (failed > 0).then(|| format!("{failed} failed"))
        },
        Some(CmdReport::Build(report)) => (!report.errors.is_empty())
            .then(|| vterm::counted(report.errors.len(), "error", "errors")),
        None => None,
    };
    let (mark, mark_scope) = match (failed, cmd.exit_status) {
        (Some(failed), _) => (format!("\u{2717} {failed}"), crate::theme::scope::UI_ERROR),
        (None, Some(0)) => (
            "\u{2713}".to_owned(),
            crate::theme::scope::UI_BADGE_COMPLETE,
        ),
        (None, Some(code)) => (
            format!("\u{2717} exit {code}"),
            crate::theme::scope::UI_ERROR,
        ),
        (None, None) => ("\u{2717}".to_owned(), crate::theme::scope::UI_ERROR),
    };
    parts.push(StatusPart {
        text: mark,
        style: theme.get(mark_scope),
        chip: false,
    });

    if let Some((started, ended)) = cmd.started.zip(cmd.ended) {
        let runtime = crate::run::format_runtime(ended.saturating_duration_since(started));
        let runtime = match cmd.finished_locations() {
            Some(locations) => {
                let matches = vterm::counted(locations.len(), "match", "matches");
                format!("{matches} \u{b7} {runtime}")
            },
            None => runtime,
        };
        let text = match cmd.run_count {
            0 | 1 => runtime,
            count => format!("{runtime} \u{b7} run {count}"),
        };
        parts.push(StatusPart {
            text,
            style: theme.get(crate::theme::scope::UI_TEXT_MUTED),
            chip: false,
        });
    }
    parts
}

/// The tag that leads a status, the Kind Chip `chip`, else the Source Tag
/// `source`, or `None` for neither.
///
/// The chip draws in `ui.run.kind`, or in `ui.key_label` for a theme that
/// leaves that unset, which draws the label with no box. The Source Tag draws
/// in its scope, and only where the theme sets the scope. A Run Block's
/// header and the Run pane prompt lead with the same tag for the same line.
fn lead_part(
    chip: Option<&'static str>,
    source: Option<(&'static str, &'static str)>,
    theme: &crate::theme::Theme,
) -> Option<StatusPart> {
    let kind_chip = chip.map(|label| StatusPart {
        text: label.to_owned(),
        style: theme
            .try_get(crate::theme::scope::UI_RUN_KIND)
            .unwrap_or_else(|| theme.get(crate::theme::scope::UI_KEY_LABEL)),
        chip: true,
    });
    kind_chip.or_else(|| {
        let (label, scope) = source?;
        theme.try_get(scope).map(|style| StatusPart {
            text: label.to_owned(),
            style,
            chip: true,
        })
    })
}

/// Paint row `row_idx` of `grid` at `y`, from `area.x` across as much of the
/// grid's width as `area` holds.
///
/// A row holds only the cells written to it. A selection also covers the
/// columns past them, so a row of a selected block paints full width. Those
/// columns show as reversed blanks.
///
/// A cell that the program left without a foreground takes the foreground of
/// `dim`, and a cell that the program colored keeps its color.
pub(super) fn paint_grid_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    grid: &VtermGrid,
    row_idx: usize,
    selection: Option<&GridSelection>,
    dim: Style,
) {
    let row = grid.row(row_idx);
    let w = (area.width as usize).min(grid.width() as usize);
    let cols = match selection {
        Some(_) => w,
        None => row.len().min(w),
    };
    let unwritten = StyledCell::default();
    let row_u16 = u16::try_from(row_idx).unwrap_or(u16::MAX);

    for col in 0..cols {
        let cell = row.get(col).unwrap_or(&unwritten);
        let col_u16 = u16::try_from(col).unwrap_or(u16::MAX);
        let selected = selection.is_some_and(|sel| sel.contains(col_u16, row_u16));
        let blank =
            cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty();
        if blank && !selected {
            continue;
        }
        let mut style = Style::default();
        if let Some(fg) = cell.fg.or(dim.fg) {
            style = style.fg(fg);
        }
        if let Some(bg) = cell.bg {
            style = style.bg(bg);
        }
        style = style.add_modifier(cell.modifiers);
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let x = area.x + col as u16;
        if x < area.x + area.width {
            buf[(x, y)].set_char(cell.ch).set_style(style);
        }
    }
}

/// Paint `row` into `area` span by span, cut at `area`'s right edge.
///
/// Each span draws in the foreground of its tone's scope alone, so a scope
/// that carries a background leaves the pane's showing.
pub(super) fn paint_body_row(
    buf: &mut Buffer,
    area: Rect,
    row: &BodyRow,
    theme: &crate::theme::Theme,
) {
    let end = area.x.saturating_add(area.width);
    let mut x = area.x;
    for (text, tone) in &row.spans {
        let style = Style {
            fg: theme.get(tone.scope()).fg,
            ..Style::default()
        };
        write_str_clipped(buf, x, area.y, text, style, end);
        x = x.saturating_add(u16::try_from(text.chars().count()).unwrap_or(u16::MAX));
    }
}

/// The block-local row offsets that fall inside `window`, for a block occupying
/// `base..base + span` of the pane's output rows.
///
/// Empty for a block wholly outside the window, which is what lets the caller
/// walk every block without materializing the rows between them.
fn window_offsets(base: usize, span: usize, window: &Range<usize>) -> Range<usize> {
    let first = window.start.saturating_sub(base);
    let last = window.end.saturating_sub(base).min(span);
    first..last.max(first)
}

/// The line drawn at `offset` within a block's span, where `content` is what
/// the block's body shows and `hint` the row the newest block carries.
///
/// The span is the prompt, then the body, then an error line if the block has
/// one, then the hint, so this and [`crate::run::Cmd::rendered_line_span`],
/// with one row for a hint, describe the same rows and have to agree. `None`
/// for an offset past them, which the caller's window bounds already exclude.
fn block_line<'a>(
    block: &'a Cmd,
    content: Body<'a>,
    hint: Option<&'a BodyRow>,
    offset: usize,
) -> Option<OutputLine<'a>> {
    let Some(row) = offset.checked_sub(1) else {
        return Some(OutputLine::Prompt {
            cwd: &block.cwd,
            command: block.command.as_str(),
        });
    };
    if row < content.rows() {
        return Some(match content {
            Body::Grid(_) => OutputLine::GridRow(&block.grid, row, block.selection.as_ref()),
            Body::Report(rows) => OutputLine::Row(&rows[row]),
        });
    }

    block
        .error
        .as_deref()
        .map(OutputLine::Error)
        .into_iter()
        .chain(hint.map(OutputLine::Row))
        .nth(row - content.rows())
}

enum OutputLine<'a> {
    Prompt {
        cwd: &'a Path,
        command: &'a str,
    },
    GridRow(&'a VtermGrid, usize, Option<&'a GridSelection>),
    Error(&'a str),
    /// A row of the block's report, or the hint row after the newest block.
    Row(&'a BodyRow),
}

/// What a block's body shows, between its header and its error line.
#[derive(Clone, Copy)]
enum Body<'a> {
    /// This many rows of the block's grid.
    Grid(usize),
    /// The rows of the block's report.
    Report(&'a [BodyRow]),
}

impl Body<'_> {
    /// The rows that the body takes.
    fn rows(self) -> usize {
        match self {
            Body::Grid(rows) => rows,
            Body::Report(rows) => rows.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_line, paint_grid_row, render_run_pane, status_parts, text_run, window_offsets, Body,
        BodyRow, Buffer, Cmd, CmdId, GridSelection, Modifier, OutputLine, Rect, RowMetrics, RunBox,
        StatusPart, Style,
    };
    use crate::{
        app::Stoat,
        pane::{PaneId, View},
        render::paint::style_rgb,
        run::{cargo::ONE_FAILURE, kind},
        test_harness::TestHarness,
        theme::{scope, Theme, FULL_CELL_SCALE},
        workspace::Workspace,
    };
    use ratatui::style::Color;
    use std::{
        ops::Range,
        path::PathBuf,
        time::{Duration, Instant},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::{
        command::{self, Command, TextRunCommand},
        INK_PROTOCOL,
    };

    /// The `(block, offset)` pairs the pane draws, built the obvious way: lay
    /// every block's rows out flat, then take the window out of the middle.
    fn flat_oracle(spans: &[usize], start: usize, visible: usize) -> Vec<(usize, usize)> {
        let rows: Vec<(usize, usize)> = spans
            .iter()
            .enumerate()
            .flat_map(|(block, span)| (0..*span).map(move |offset| (block, offset)))
            .collect();
        rows.into_iter().skip(start).take(visible).collect()
    }

    /// The same pairs, walked block by block the way the renderer does.
    fn walked(spans: &[usize], start: usize, visible: usize) -> Vec<(usize, usize)> {
        let window = start..start + visible;
        let mut base = 0;
        let mut rows = Vec::new();
        for (block, span) in spans.iter().enumerate() {
            rows.extend(window_offsets(base, *span, &window).map(|offset| (block, offset)));
            base += span;
        }
        rows
    }

    #[test]
    fn the_walked_window_matches_a_flat_layout_of_the_same_blocks() {
        let spans = [3, 1, 40, 2, 7, 1, 12];
        let total: usize = spans.iter().sum();

        for start in 0..=total {
            for visible in [0, 1, 5, 24, total] {
                assert_eq!(
                    walked(&spans, start, visible),
                    flat_oracle(&spans, start, visible),
                    "window {start}..{} over {spans:?}",
                    start + visible
                );
            }
        }
    }

    /// Names the line without needing the block it borrows from.
    fn tag(line: Option<OutputLine<'_>>) -> String {
        match line {
            None => "none".to_owned(),
            Some(OutputLine::Prompt { .. }) => "prompt".to_owned(),
            Some(OutputLine::GridRow(_, row, _)) => format!("grid{row}"),
            Some(OutputLine::Error(_)) => "error".to_owned(),
            Some(OutputLine::Row(row)) => row.text(),
        }
    }

    fn block_with(rows: &str, error: Option<&str>) -> Cmd {
        let mut block = Cmd::new("cmd".to_owned(), PathBuf::from("/repo"), 20);
        block.grid.feed(rows.as_bytes());
        block.error = error.map(str::to_owned);
        block
    }

    #[test]
    fn a_blocks_span_reads_prompt_then_grid_rows_then_its_error() {
        let block = block_with("a\r\nb\r\nc", Some("boom"));
        let grid_rows = block.grid.rendered_line_count();
        let span = block.rendered_line_span();

        let lines: Vec<String> = (0..span)
            .map(|offset| tag(block_line(&block, Body::Grid(grid_rows), None, offset)))
            .collect();

        assert_eq!(
            lines,
            ["prompt", "grid0", "grid1", "grid2", "error"],
            "each offset maps to its own row, the grid starting one past the prompt"
        );
    }

    #[test]
    fn a_block_without_an_error_ends_at_its_last_grid_row() {
        let block = block_with("a\r\nb", None);
        let grid_rows = block.grid.rendered_line_count();
        let span = block.rendered_line_span();

        let lines: Vec<String> = (0..span)
            .map(|offset| tag(block_line(&block, Body::Grid(grid_rows), None, offset)))
            .collect();

        assert_eq!(
            lines,
            ["prompt", "grid0", "grid1"],
            "the span stops at the grid when there is no error line to follow it"
        );
    }

    #[test]
    fn a_selection_past_a_rows_content_paints_reversed_blanks() {
        let block = block_with("ab", None);
        let selection = GridSelection {
            anchor: (0, 0),
            head: (5, 0),
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_grid_row(
            &mut buf,
            area,
            0,
            &block.grid,
            0,
            Some(&selection),
            Style::default(),
        );

        let reversed: Vec<u16> = (0..20)
            .filter(|&x| buf[(x, 0)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert_eq!(
            reversed,
            [0, 1, 2, 3, 4, 5],
            "the selection reaches past the two written cells"
        );
    }

    #[test]
    fn a_block_outside_the_window_contributes_nothing() {
        let window = 10..20;
        assert_eq!(
            [
                window_offsets(0, 5, &window).count(),
                window_offsets(30, 5, &window).count(),
                window_offsets(8, 5, &window).count(),
            ],
            [0, 0, 3],
            "a block before or after the window is skipped, one straddling it is clipped"
        );
    }

    /// Inside stoatty a hairline in the prompt rule's color rules the prompt
    /// row off the stack, along the row's top edge and across the inset that
    /// the stack stands in.
    #[test]
    fn the_prompt_rule_spans_the_inset_width() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        h.render_composited();

        let content = pane_area(&h);
        let rule = style_rgb(h.stoat.theme.get(scope::UI_RUN_PROMPT_RULE).fg)
            .expect("an RGB prompt rule color");
        let prompt_row = content.y + content.height - 1;
        let hairlines: Vec<_> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar)
                    if bar.height == 1
                        && bar.color == rule
                        && bar.y == (prompt_row * 16) as i16 =>
                {
                    Some((bar.x, bar.width))
                },
                _ => None,
            })
            .collect();

        assert_eq!(
            hairlines,
            [(((content.x + 1) * 16) as i16, (content.width - 2) * 16)],
        );
    }

    /// A theme with the colors of a Run Block's status, finished or running.
    fn status_theme() -> Theme {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.badge.complete.fg = "#8fd18a";
                ui.error.fg = "#f2777f";
                ui.text.muted.fg = "#838a98";
                ui.statusline.run.fg = "#c59af0";
            }"##,
        );
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// A Cmd that finished with `exit_status` after running for `ran`, on its
    /// `run_count`th run.
    fn finished(exit_status: Option<i32>, ran: Duration, run_count: u32) -> Cmd {
        let mut cmd = block_with("", None);
        let started = Instant::now();
        cmd.started = Some(started);
        cmd.ended = Some(started + ran);
        cmd.finished = true;
        cmd.exit_status = exit_status;
        cmd.run_count = run_count;
        cmd
    }

    /// The text and the foreground of each part of `cmd`'s status.
    fn status(cmd: &Cmd, theme: &Theme) -> Vec<(String, Option<Color>)> {
        status_parts(cmd, &slotmap::SlotMap::with_key(), theme)
            .into_iter()
            .map(|part| (part.text, part.style.fg))
            .collect()
    }

    /// The area of the focused pane of `h`'s active workspace.
    fn pane_area(h: &TestHarness) -> Rect {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    }

    /// A Cmd that ran 18 ms to an exit of 0 reads a mark and a runtime, and the
    /// pane draws both as runs on the header's row, the runtime ending at the
    /// right edge of the stack. A lone header with no output sits on the prompt.
    #[test]
    fn a_finished_header_ends_in_its_mark_and_runtime() {
        let theme = status_theme();
        let parts = status(&finished(Some(0), Duration::from_millis(18), 1), &theme);

        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        let handle = h.submit_run("true");
        h.advance_clock(Duration::from_millis(18));
        h.inject_cmd_done(handle.cmd, 0);
        h.render_composited();
        let area = pane_area(&h);
        let runs: Vec<_> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::TextRun(run) if ["\u{2713}", "18ms"].contains(&run.text.as_str()) => {
                    let end = run.col + 16 * run.text.chars().count() as i16;
                    Some((run.text, run.row, end))
                },
                _ => None,
            })
            .collect();

        let header = area.bottom() - 2;
        let right = crate::run::stack_rect(area).right();
        let (row, right) = ((header * 16) as i16, (right * 16) as i16);
        assert_eq!(
            (parts, runs),
            (
                vec![
                    (
                        "\u{2713}".to_owned(),
                        theme.get(scope::UI_BADGE_COMPLETE).fg
                    ),
                    ("18ms".to_owned(), theme.get(scope::UI_TEXT_MUTED).fg),
                ],
                vec![
                    ("\u{2713}".to_owned(), row, right - 64 - 14),
                    ("18ms".to_owned(), row, right),
                ],
            ),
        );
    }

    #[test]
    fn a_failed_header_reads_its_exit_code() {
        let theme = status_theme();
        let error = theme.get(scope::UI_ERROR).fg;
        let marks = [Some(5), None]
            .map(|exit| status(&finished(exit, Duration::ZERO, 1), &theme).remove(0));

        assert_eq!(
            marks,
            [
                ("\u{2717} exit 5".to_owned(), error),
                ("\u{2717}".to_owned(), error)
            ],
        );
    }

    #[test]
    fn a_later_run_names_its_count() {
        let cmd = finished(Some(0), Duration::from_secs(151), 3);
        let texts: Vec<String> = status(&cmd, &status_theme())
            .into_iter()
            .map(|(text, _)| text)
            .collect();

        assert_eq!(texts, ["\u{2713}", "2:31 \u{b7} run 3"]);
    }

    /// A running Cmd reads U+25CF and the whole seconds that the last tick
    /// wrote, in the run color.
    #[test]
    fn a_running_header_reads_its_elapsed_time() {
        let mut cmd = finished(None, Duration::from_secs(1), 1);
        cmd.finished = false;
        cmd.ended = None;
        cmd.elapsed_secs = 21;
        let theme = status_theme();

        assert_eq!(
            status(&cmd, &theme),
            [(
                "\u{25cf} 0:21".to_owned(),
                theme.get(scope::UI_STATUSLINE_RUN).fg
            )],
        );
    }

    /// The header of `echo ok` draws `echo` as the command token and `ok` as
    /// plain text, after the cwd `/w` and the glyph. The blank between them is
    /// not read, since the block's rule composites its line glyph into the
    /// blank cells of the header row.
    #[test]
    fn a_header_colors_its_first_word() {
        let mut h = TestHarness::with_size(60, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/w");
        h.open_run();
        h.submit_run("echo ok");
        let buf = h.render_composited();

        let stack = crate::run::stack_rect(pane_area(&h));
        let header = stack.bottom() - 2;
        let theme = &h.stoat.theme;
        let looks = |cols: Range<u16>| {
            cols.map(|col| {
                let cell = &buf[(stack.x + col, header)];
                (cell.fg, cell.modifier)
            })
            .collect::<Vec<_>>()
        };
        let look = |style: Style, cells: usize| {
            vec![(style.fg.unwrap_or(Color::Reset), style.add_modifier); cells]
        };
        assert_eq!(
            (looks(5..9), looks(10..12)),
            (
                look(theme.get(scope::UI_CMD_COMMAND), 4),
                look(theme.get(scope::UI_TEXT), 2)
            ),
        );
    }

    /// A failed block names its exit in its status alone, so neither its own
    /// header, the header after it, nor the prompt row holds a `[N]` flag.
    #[test]
    fn no_row_carries_an_exit_flag() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        let failed = h.submit_run("false");
        h.inject_cmd_done(failed.cmd, 5);
        h.submit_run("retry");
        let buf = h.render_composited();

        let area = pane_area(&h);
        let flagged: Vec<String> = (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .filter(|row: &String| row.contains('['))
            .collect();
        assert_eq!(flagged, Vec::<String>::new());
    }

    /// A bar of a scene as its x, y, width, height, and color.
    type BarSpec = (i16, i16, u16, u16, [u8; 3]);

    const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
    const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
    const SPECIAL: [u8; 3] = [0xc5, 0x9a, 0xf0];
    const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];

    /// A harness at `/w` whose Run pane holds a block for each of `runs`: its
    /// line, its output, and its exit code, or `None` for a Cmd that runs on.
    fn stacked(runs: &[(&str, &[u8], Option<i32>)]) -> TestHarness {
        let mut h = TestHarness::with_size(60, 20);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/w");
        h.open_run();
        for &(line, output, exit) in runs {
            let handle = h.submit_run(line);
            h.inject_cmd_output(handle.cmd, output);
            if let Some(code) = exit {
                h.inject_cmd_done(handle.cmd, code);
            }
        }
        h
    }

    /// Paint the Run pane of `h` into `area` under `theme`, through a live
    /// scene, on a buffer that ends where `area` does.
    fn painted(h: &mut TestHarness, theme: &Theme, area: Rect) -> (Buffer, ApcScene) {
        let Workspace {
            runs,
            cmds,
            terms,
            editors,
            git_root,
            ..
        } = h.stoat.active_workspace_mut();
        let run = runs.values_mut().next().expect("an open run");
        let mut buf = Buffer::empty(Rect::new(0, 0, area.right(), area.bottom()));
        let mut scene = ApcScene::new();
        render_run_pane(
            run, cmds, terms, editors, theme, git_root, None, area, false, None, &mut buf,
            &mut scene,
        );
        (buf, scene)
    }

    /// The text of row `y` of `buf`, its trailing blanks dropped.
    fn row_text(buf: &Buffer, y: u16) -> String {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.trim_end().to_owned()
    }

    /// Each bar of `scene`.
    fn bars(scene: &ApcScene) -> Vec<BarSpec> {
        command::decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) => Some((bar.x, bar.y, bar.width, bar.height, bar.color)),
                _ => None,
            })
            .collect()
    }

    /// A stack shorter than the pane sits on the prompt and leaves its blank
    /// rows at the top.
    #[test]
    fn a_short_stack_sits_on_the_prompt() {
        let mut h = stacked(&[("ls", b"one\r\ntwo\r\n", None)]);
        let theme = h.stoat.theme.clone();
        let (buf, _) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        let rows: Vec<String> = (0..12).map(|y| row_text(&buf, y)).collect();
        let mut want = vec![String::new(); 8];
        want.extend([" /w \u{276f} ls", "  one", "  two", " /w \u{276f}"].map(String::from));
        assert_eq!(rows, want);
    }

    #[test]
    fn output_starts_one_cell_inside_its_header() {
        let mut h = stacked(&[("ls", b"one\r\n", None)]);
        let theme = h.stoat.theme.clone();
        let (buf, _) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        let first_glyph = |y: u16| (0..40).find(|&x| buf[(x, y)].symbol() != " ");
        assert_eq!([first_glyph(9), first_glyph(10)], [Some(1), Some(2)]);
    }

    #[test]
    fn uncolored_output_draws_dim_and_colored_output_keeps_its_color() {
        let mut h = stacked(&[("ls", b"plain\r\n\x1b[31mred", None)]);
        let theme = h.stoat.theme.clone();
        let (buf, _) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        let colors = |y: u16, cols: Range<u16>| cols.map(|x| buf[(x, y)].fg).collect::<Vec<_>>();
        let dim = theme
            .get(scope::UI_TEXT_DIM)
            .fg
            .expect("the theme sets a dim color");
        assert_eq!(
            (colors(9, 2..7), colors(10, 2..5)),
            (vec![dim; 5], vec![Color::Red; 3]),
        );
    }

    /// Each block ends in a rule along its last row's bottom edge, and a rule
    /// along the prompt row's top edge rules the prompt off, each across the
    /// stack's inset.
    #[test]
    fn each_block_ends_in_a_rule_and_the_prompt_starts_with_one() {
        let mut h = stacked(&[("l1", b"one\r\n", Some(0)), ("l2", b"", Some(0))]);
        let theme = h.stoat.theme.clone();
        let (_, scene) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        assert_eq!(
            bars(&scene),
            [
                (16, 9 * 16 + 15, 38 * 16, 1, RAISED),
                (16, 10 * 16 + 15, 38 * 16, 1, RAISED),
                (16, 11 * 16, 38 * 16, 1, HAIRLINE),
            ],
        );
    }

    /// A failed block draws a danger bar beside its body rows. A block that
    /// exits 0 and a failed block with no output draw none.
    #[test]
    fn a_failed_body_carries_the_danger_bar() {
        let mut h = stacked(&[
            ("ok", b"x\r\n", Some(0)),
            ("bad", b"one\r\ntwo\r\n", Some(1)),
            ("quiet", b"", Some(1)),
        ]);
        let theme = h.stoat.theme.clone();
        let (_, scene) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        let danger = style_rgb(theme.get(scope::UI_ERROR).fg).expect("an RGB error color");
        let marks: Vec<BarSpec> = bars(&scene)
            .into_iter()
            .filter(|&(.., width, _, _)| width == 4)
            .collect();
        assert_eq!(marks, [(16, 8 * 16, 4, 32, danger)]);
    }

    /// A finished `cargo test` block draws its report's rows as its body, and
    /// the hint row after them is its last row, on the prompt, with the
    /// danger bar beside it and the block's rule under it.
    #[test]
    fn a_report_block_ends_in_the_hint_row() {
        let mut h = stacked(&[("cargo test", ONE_FAILURE.as_bytes(), Some(101))]);
        let theme = h.stoat.theme.clone();
        let (buf, scene) = painted(&mut h, &theme, Rect::new(0, 0, 80, 12));

        let lines = h
            .stoat
            .active_workspace()
            .cmds
            .values()
            .next()
            .expect("a Cmd")
            .grid
            .rendered_line_count();
        let rows: Vec<String> = (6..12).map(|y| row_text(&buf, y)).collect();
        let danger = style_rgb(theme.get(scope::UI_ERROR).fg).expect("an RGB error color");
        let edges: Vec<BarSpec> = bars(&scene)
            .into_iter()
            .filter(|&(.., width, _, color)| width == 4 || color == RAISED)
            .collect();
        assert_eq!(
            (rows, edges),
            (
                vec![
                    "  1 passed  1 failed  3 tests \u{b7} 1 binary".to_owned(),
                    "  \u{2717} render::chrome::tests::zoomed_negative_steps   chrome.rs:412"
                        .to_owned(),
                    "    assertion `left == right` failed  left: 18  right: 16".to_owned(),
                    format!("  \u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    "  C-r run again \u{b7} Enter go to failure".to_owned(),
                    " /w \u{276f}".to_owned(),
                ],
                vec![
                    (16, 6 * 16, 4, 5 * 16, danger),
                    (16, 10 * 16 + 15, 78 * 16, 1, RAISED),
                ],
            ),
        );
    }

    /// A theme that leaves the run scopes unset draws no rule under a block and
    /// the prompt's rule in its divider color.
    #[test]
    fn a_theme_without_the_run_scopes_draws_the_divider_hairline_alone() {
        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "default_dark")
            .expect("the shipped theme builds");
        let mut h = stacked(&[("a", b"one\r\n", Some(0))]);
        let (_, scene) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));

        let divider = style_rgb(theme.get(scope::UI_DIVIDER).fg).expect("an RGB divider color");
        assert_eq!(bars(&scene), [(16, 11 * 16, 38 * 16, 1, divider)]);
    }

    /// A Parsed Cmd's status leads with its Kind Chip in the `ui.run.kind`
    /// colors, before its mark and its runtime. A theme that leaves the scope
    /// unset gives the chip the `ui.key_label` foreground and no background, so
    /// the chip draws as a label with no box.
    #[test]
    fn a_parsed_header_leads_its_status_with_the_kind_chip() {
        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let config = config.expect("the shipped config parses");
        let shipped =
            |name: &str| Theme::from_config(&config, name).expect("the shipped theme builds");
        let (ermine, dark) = (shipped("ermine_night"), shipped("default_dark"));
        let mut cmd = finished(Some(0), Duration::ZERO, 1);
        cmd.kind = kind::run_kind("cargo test");

        let terms = slotmap::SlotMap::with_key();
        let ermine_parts = status_parts(&cmd, &terms, &ermine);
        let dark_parts = status_parts(&cmd, &terms, &dark);
        let texts: Vec<_> = ermine_parts
            .iter()
            .map(|part| (part.text.as_str(), part.chip))
            .collect();
        let chip_colors = |parts: &[StatusPart]| (parts[0].style.fg, parts[0].style.bg);
        assert_eq!(
            (texts, chip_colors(&ermine_parts), chip_colors(&dark_parts)),
            (
                vec![("cargo test", true), ("\u{2713}", false), ("0ms", false)],
                (
                    Some(Color::Rgb(0x6c, 0xc7, 0xd6)),
                    Some(Color::Rgb(0x1b, 0x2a, 0x2d))
                ),
                (dark.get(scope::UI_KEY_LABEL).fg, None),
            ),
        );
    }

    /// A block in Jobs reads as a Stub. Its status names the Job in the run
    /// color, one row stands in place of its output, and its hint row names
    /// the key that brings it back.
    #[test]
    fn a_block_in_jobs_reads_as_a_stub() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_output(job.cmd, b"hello\n");
        h.advance_clock(Duration::from_secs(41));
        h.snapshot();

        let ws = h.stoat.active_workspace();
        let run = ws.runs.values().next().expect("an open run");
        let body = ws.cmds[job.cmd]
            .report_rows()
            .map(|rows| rows.iter().map(toned).collect::<Vec<_>>());
        assert_eq!(
            (
                h.cmd_status(job.cmd),
                last_part_rgb(&h, job.cmd),
                body,
                run.hint(&ws.cmds, &ws.terms).as_ref().map(toned),
                h.rendered_text().contains("hello"),
            ),
            (
                "\u{25cf} in Jobs \u{b7} 0:41".to_owned(),
                Some(SPECIAL),
                Some(vec![vec![(
                    "pushed to Jobs with C-j \u{b7} the Tray Chip carries it".to_owned(),
                    "Muted".to_owned(),
                )]]),
                Some(vec![
                    ("Enter".to_owned(), "Key".to_owned()),
                    (" go to the Job \u{b7} ".to_owned(), "Muted".to_owned()),
                    ("C-j".to_owned(), "Key".to_owned()),
                    (" bring it back here".to_owned(), "Muted".to_owned()),
                ]),
                false,
            ),
        );
    }

    /// The Stub of a Job carries a bar in the color of its state beside its
    /// row and its hint row.
    #[test]
    fn the_rows_of_a_stub_carry_a_two_pixel_bar() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");

        assert_eq!(stub_bars(&mut h), [(16, 9 * 16, 4, 32, SPECIAL)]);
    }

    /// A Job that fails in the tray shows its result on its Stub, its status
    /// and its bar in the failed color.
    #[test]
    fn the_stub_of_a_failed_job_shows_the_result() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_done(job.cmd, 101);

        assert_eq!(
            (
                h.cmd_status(job.cmd),
                last_part_rgb(&h, job.cmd),
                stub_bars(&mut h)
            ),
            (
                "\u{2717} in Jobs \u{b7} exit 101".to_owned(),
                Some(DANGER),
                vec![(16, 9 * 16, 4, 32, DANGER)],
            ),
        );
    }

    /// The hint row belongs to the newest block alone, so a Stub under a
    /// newer block has none, and its bar runs beside its one row.
    #[test]
    fn a_stub_under_a_newer_block_has_no_hint_row() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.submit_run("echo a");

        let hint = {
            let ws = h.stoat.active_workspace();
            let run = ws.runs.values().next().expect("an open run");
            run.hint(&ws.cmds, &ws.terms).as_ref().map(toned)
        };
        let bars: Vec<(u16, [u8; 3])> = stub_bars(&mut h)
            .into_iter()
            .map(|(.., height, color)| (height, color))
            .collect();
        assert_eq!((hint, bars), (None, vec![(16, SPECIAL)]));
    }

    /// A running Job whose report counts its progress reads the counts on its
    /// Stub in place of its time, and its Stub draws no progress bar.
    #[test]
    fn a_stub_with_a_progress_reads_its_counts() {
        let mut h = Stoat::test();
        let job = h.push_job("cargo test");
        h.inject_cmd_output(
            job.cmd,
            b"running 4 tests\r\ntest a ... ok\r\ntest b ... ok\r\n",
        );
        let theme = h.stoat.theme.clone();
        let (_, scene) = painted(&mut h, &theme, Rect::new(0, 0, 40, 12));
        let progress: Vec<BarSpec> = bars(&scene)
            .into_iter()
            .filter(|&(.., height, _)| height == 2)
            .collect();

        assert_eq!(
            (h.cmd_status(job.cmd), progress),
            (
                "cargo test \u{25cf} in Jobs \u{b7} 2 / 4".to_owned(),
                Vec::new()
            ),
        );
    }

    /// The keys of the pane that act on its newest block pass a block in Jobs.
    /// `ctrl-c` interrupts the older running Cmd, and `ctrl-r` finds no ended
    /// Cmd to run again. `enter` and `t` leave the ended Job as it was, though
    /// its report would take both.
    #[test]
    fn the_keys_of_the_pane_pass_a_block_in_jobs() {
        let mut h = Stoat::test();
        h.open_run();
        let other = h.submit_run("sleep 31");
        let job = h.push_job("cargo test");
        h.type_keys("ctrl-c");
        let interrupts = [&other, &job].map(|handle| handle.session.sent_bytes());

        h.inject_cmd_output(job.cmd, ONE_FAILURE.as_bytes());
        h.inject_cmd_done(job.cmd, 101);
        let spawns = h.fake_terminal_host().spawns().len();
        h.type_keys("ctrl-r");
        let respawns = h.fake_terminal_host().spawns().len() - spawns;

        h.stoat.pending_message = None;
        let views = pane_views(&h);
        h.type_keys("enter");
        let entered = (pane_views(&h) == views, h.stoat.pending_message.clone());
        h.type_keys("escape t");

        assert_eq!(
            (
                interrupts,
                respawns,
                entered,
                h.stoat.active_workspace().cmds[job.cmd].raw_view,
            ),
            ([vec![vec![0x03]], Vec::new()], 0, (true, None), false),
        );
    }

    /// The text and the tone of each span of `row`, the tone by its name.
    fn toned(row: &BodyRow) -> Vec<(String, String)> {
        row.spans
            .iter()
            .map(|(text, tone)| (text.clone(), format!("{tone:?}")))
            .collect()
    }

    /// The RGB foreground of the last part of the status of the Cmd `cmd`.
    fn last_part_rgb(h: &TestHarness, cmd: CmdId) -> Option<[u8; 3]> {
        let ws = h.stoat.active_workspace();
        status_parts(&ws.cmds[cmd], &ws.terms, &h.stoat.theme)
            .last()
            .and_then(|part| style_rgb(part.style.fg))
    }

    /// The bars 4 sixteenths wide of the Run pane of `h`, painted into an area
    /// of 40 by 12 cells.
    fn stub_bars(h: &mut TestHarness) -> Vec<BarSpec> {
        let theme = h.stoat.theme.clone();
        let (_, scene) = painted(h, &theme, Rect::new(0, 0, 40, 12));
        bars(&scene)
            .into_iter()
            .filter(|&(.., width, _, _)| width == 4)
            .collect()
    }

    /// The view of each split pane of `h` and the focused pane.
    fn pane_views(h: &TestHarness) -> (Vec<View>, PaneId) {
        let panes = &h.stoat.active_workspace().panes;
        let views = panes
            .split_panes()
            .map(|(_, pane)| pane.view.clone())
            .collect();
        (views, panes.focus())
    }

    const M1: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };
    const PIXEL_DIM: [u8; 3] = [4, 4, 4];
    const PIXEL_ERROR: [u8; 3] = [6, 6, 6];
    const PIXEL_RULE: [u8; 3] = [8, 8, 8];
    const PIXEL_PROMPT_RULE: [u8; 3] = [9, 9, 9];

    /// The area of the first box, 60 cells by 12 under an Island flush with it.
    const PIXEL_AREA: Rect = Rect::new(0, 0, 60, 12);

    /// A theme with the plate's two surfaces, the run scopes, and the colors of
    /// a block's lines, for a stack at the board's pixels.
    fn pixel_theme() -> Theme {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.surface.ground.bg = "#010101";
                ui.background.bg = "#020202";
                ui.text.fg = "#030303";
                ui.text.dim.fg = "#040404";
                ui.text.muted.fg = "#050505";
                ui.error.fg = "#060606";
                ui.badge.complete.fg = "#070707";
                ui.run.rule.fg = "#080808";
                ui.run.prompt.rule.fg = "#090909";
                ui.cmd.cwd.fg = "#0b0b0b";
                ui.divider.fg = "#0c0c0c";
                ui.statusline.run.fg = "#0d0d0d";
            }"##,
        );
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// Paint the Run pane of `h` into [`PIXEL_AREA`] at the board's pixels of
    /// `M1` under [`pixel_theme`], through a scene that reads nudges and inks,
    /// with the pane holding the focus where `focused` holds.
    fn pixel_painted(h: &mut TestHarness, focused: bool) -> (Buffer, ApcScene) {
        let Workspace {
            runs,
            cmds,
            terms,
            editors,
            git_root,
            ..
        } = h.stoat.active_workspace_mut();
        let run = runs.values_mut().next().expect("an open run");
        let area = PIXEL_AREA;
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        scene.set_ink_protocol(true);
        let pixels = Some((M1, [0; 4]));
        let theme = pixel_theme();
        render_run_pane(
            run, cmds, terms, editors, &theme, git_root, None, area, focused, pixels, &mut buf,
            &mut scene,
        );
        (buf, scene)
    }

    /// Each command of `scene` but the nudges, with the nudge in force where it
    /// stands.
    fn placed(scene: &ApcScene) -> Vec<([i16; 2], Command)> {
        let mut nudge = [0, 0];
        command::decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Nudge(offset) => {
                    nudge = offset;
                    None
                },
                cmd => Some((nudge, cmd)),
            })
            .collect()
    }

    /// The bars of `placed` under `nudge`.
    fn bars_under(placed: &[([i16; 2], Command)], nudge: [i16; 2]) -> Vec<BarSpec> {
        placed
            .iter()
            .filter_map(|(at, cmd)| match cmd {
                Command::Bar(bar) if *at == nudge => {
                    Some((bar.x, bar.y, bar.width, bar.height, bar.color))
                },
                _ => None,
            })
            .collect()
    }

    /// The runs of `placed` under `nudge`.
    fn runs_under(placed: &[([i16; 2], Command)], nudge: [i16; 2]) -> Vec<&TextRunCommand> {
        placed
            .iter()
            .filter_map(|(at, cmd)| match cmd {
                Command::TextRun(run) if *at == nudge => Some(run),
                _ => None,
            })
            .collect()
    }

    /// The inks, the runs as their text, col, and color, and the bars as their
    /// x, width, height, and color, that `placed` draws after the header and
    /// before the prompt, which stands under the nudge `[12, 9]` in
    /// [`PIXEL_AREA`].
    fn body_paint(placed: &[([i16; 2], Command)]) -> Vec<String> {
        let header = placed.first().map(|(at, _)| *at);
        placed
            .iter()
            .skip_while(|(at, _)| Some(*at) == header)
            .take_while(|(at, _)| *at != [12, 9])
            .filter_map(|(_, cmd)| match cmd {
                Command::Ink(ink) => Some(format!("ink {ink:?}")),
                Command::TextRun(run) => {
                    Some(format!("run {} {} {:?}", run.text, run.col, run.color))
                },
                Command::Bar(bar) => Some(format!(
                    "bar {} {} {} {:?}",
                    bar.x, bar.width, bar.height, bar.color
                )),
                _ => None,
            })
            .collect()
    }

    /// A block of `echo ok` that printed `ok` draws its header on the pixel row
    /// 138, its output on 159, its rule on 180, and the prompt's rule on 181,
    /// as runs and bars that write no cell.
    #[test]
    fn a_pixel_stack_draws_its_header_its_body_and_its_two_rules() {
        let mut h = stacked(&[("echo ok", b"ok\r\n", Some(0))]);
        let (buf, scene) = pixel_painted(&mut h, false);
        let placed = placed(&scene);

        let glyphs = (0..PIXEL_AREA.height)
            .flat_map(|y| (0..PIXEL_AREA.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].symbol() != " ")
            .count();
        let header = runs_under(&placed, [12, 12]);
        let end = |run: &&TextRunCommand| {
            let advance = text_run::advance_sixteenths(run.text.chars().count(), FULL_CELL_SCALE);
            run.col + advance as i16
        };
        let output: Vec<(&str, i16, i16, [u8; 3])> = runs_under(&placed, [12, 15])
            .into_iter()
            .map(|run| (run.text.as_str(), run.col, run.row, run.color))
            .collect();
        let lead: Vec<(&str, i16, i16)> = header
            .iter()
            .take(4)
            .map(|run| (run.text.as_str(), run.col, run.row))
            .collect();
        assert_eq!(
            (glyphs, lead, header.last().map(end), output),
            (
                0,
                vec![
                    ("/w", 0, 112),
                    ("\u{276f}", 43, 112),
                    ("echo", 70, 112),
                    (" ok", 134, 112),
                ],
                Some(917),
                vec![("ok", 18, 128, PIXEL_DIM)]
            ),
            "no glyph in a cell, the cwd, the glyph, and the input 6 px apart from the row's \
             left, the status ending at the content's right, and the output 10 px in",
        );

        let prompt_rows: Vec<i16> = runs_under(&placed, [12, 9])
            .into_iter()
            .map(|run| run.row)
            .collect();
        let last = command::decode_stream(scene.bytes()).pop();
        assert_eq!(
            (
                bars_under(&placed, [12, 0]),
                bars_under(&placed, [12, 1]),
                prompt_rows,
                last,
            ),
            (
                vec![(0, 160, 917, 1, PIXEL_RULE)],
                vec![(0, 160, 917, 1, PIXEL_PROMPT_RULE)],
                vec![160, 160],
                Some(Command::Nudge([0, 0])),
            ),
            "the block's rule, the prompt's rule, the prompt's cwd and glyph, and no nudge after",
        );
    }

    /// A cell's RGB color draws in its run, a named color under an ink of its
    /// palette entry, and a cell with no color in `ui.text.dim`.
    #[test]
    fn output_draws_the_colors_of_its_cells() {
        let output: &[u8] = b"plain\r\n\x1b[31mred\x1b[0m\r\n\x1b[38;2;1;2;3mrgb";
        let mut h = stacked(&[("ls", output, Some(0))]);
        let (_, scene) = pixel_painted(&mut h, false);

        assert_eq!(
            body_paint(&placed(&scene)),
            [
                "run plain 18 [4, 4, 4]",
                "ink Some(1)",
                "run red 18 [4, 4, 4]",
                "ink None",
                "run rgb 18 [1, 2, 3]",
            ],
        );
    }

    /// A cell's background draws as a bar one body line high under its run,
    /// and a selected cell draws that bar in its text color and its run in the
    /// surface's color.
    #[test]
    fn a_cell_background_and_a_selection_draw_a_bar_one_body_line_high() {
        let mut h = stacked(&[("ls", b"\x1b[41mX", Some(0))]);
        let (_, scene) = pixel_painted(&mut h, false);
        let unselected = body_paint(&placed(&scene));

        let block = h
            .stoat
            .active_workspace()
            .runs
            .values()
            .next()
            .expect("a run")
            .blocks[0];
        h.stoat.active_workspace_mut().cmds[block].selection = Some(GridSelection {
            anchor: (0, 0),
            head: (1, 0),
        });
        let (_, scene) = pixel_painted(&mut h, false);
        assert_eq!(
            [unselected, body_paint(&placed(&scene))],
            [
                vec![
                    "ink Some(1)",
                    "bar 18 16 15 [4, 4, 4]",
                    "ink None",
                    "run X 18 [4, 4, 4]",
                ],
                vec!["bar 18 32 15 [4, 4, 4]", "run X 18 [2, 2, 2]"],
            ],
        );
    }

    /// A failed block draws a bar 2 px wide beside its body, from the top of
    /// its first line to the bottom of its last, and a passed block draws none.
    #[test]
    fn a_failed_body_carries_a_two_pixel_bar_over_its_lines() {
        let marks = |exit: i32| {
            let mut h = stacked(&[("ls", b"one\r\ntwo\r\n", Some(exit))]);
            let (_, scene) = pixel_painted(&mut h, false);
            bars_under(&placed(&scene), [12, 16])
                .into_iter()
                .filter(|&(.., width, _, _)| width == 4)
                .collect::<Vec<BarSpec>>()
        };
        assert_eq!(
            [marks(1), marks(0)],
            [vec![(0, 112, 4, 30, PIXEL_ERROR)], Vec::new()],
        );
    }

    /// The frame draws the stack at the board's pixels only for a stoatty that
    /// reads `ink` frames. One before that keeps the cells of the header row.
    #[test]
    fn a_pane_before_the_ink_protocol_keeps_its_cells() {
        let header_cells = |protocol: u32| {
            let mut h = stacked(&[("echo ok", b"ok\r\n", Some(0))]);
            h.stoat.stoatty = true;
            h.stoat.stoatty_protocol = protocol;
            h.stoat.cell_pixels = Some((9, 18));
            h.stoat.display_scale = Some(1.0);
            let buf = h.stoat.render();
            let area = pane_area(&h);
            let row = area.bottom() - 3;
            (area.x..area.right())
                .filter(|&x| buf[(x, row)].symbol() != " ")
                .count()
                > 0
        };
        assert_eq!(
            [header_cells(INK_PROTOCOL - 1), header_cells(INK_PROTOCOL)],
            [true, false],
        );
    }

    /// A pixel paint keeps the box it drew with, and a cell paint clears it.
    #[test]
    fn the_paint_stores_its_box() {
        let mut h = stacked(&[("ls", b"x\r\n", Some(0))]);
        let stored = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .runs
                .values()
                .next()
                .and_then(|run| run.pixel_box)
        };
        pixel_painted(&mut h, false);
        let pixel = stored(&h);
        let theme = h.stoat.theme.clone();
        painted(&mut h, &theme, PIXEL_AREA);
        assert_eq!(
            [pixel, stored(&h)],
            [
                Some(RunBox::of(M1, FULL_CELL_SCALE, PIXEL_AREA, [0; 4])),
                None
            ],
        );
    }

    /// A running test's progress bar draws on the top of its first body line,
    /// the pixel row 142, from 10 px in to the content's right, and its fill
    /// covers the share of the tests that finished.
    #[test]
    fn a_running_test_draws_its_progress_bar_from_the_body_start() {
        let output: &[u8] = b"running 4 tests\r\ntest a ... ok\r\n";
        let mut h = stacked(&[("cargo test", output, None)]);
        let (_, scene) = pixel_painted(&mut h, false);

        let progress: Vec<BarSpec> = bars_under(&placed(&scene), [12, 16])
            .into_iter()
            .filter(|&(.., height, _)| height == 2)
            .collect();
        assert_eq!(
            progress,
            [
                (18, 112, 899, 2, [12, 12, 12]),
                (18, 112, 224, 2, [13, 13, 13])
            ],
        );
    }

    /// A focused pane's prompt draws its caret where its input starts, as a bar
    /// one char wide in the text color, and an unfocused one draws none.
    #[test]
    fn a_focused_prompt_draws_its_caret_where_its_input_starts() {
        let carets = |focused| {
            let mut h = stacked(&[]);
            let (_, scene) = pixel_painted(&mut h, focused);
            bars_under(&placed(&scene), [12, 9])
        };
        assert_eq!(
            [carets(true), carets(false)],
            [vec![(70, 161, 16, 13, [3, 3, 3])], Vec::new()],
        );
    }
}
