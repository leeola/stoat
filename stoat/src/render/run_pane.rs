use crate::{
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
        cmd_line::{self, CmdLine, CmdStatus},
        paint::style_rgb,
        picker::{Footer, PlateHeader},
        text::{write_str, write_str_clipped},
    },
    run::{
        cargo::CmdReport,
        vterm::{self, BodyRow},
        Cmd, CmdId, GridSelection, RunState, StyledCell, VtermGrid,
    },
    term_session::{TermId, TermSession},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use std::{ops::Range, path::Path};
use stoat_widgets::bar::Bar;

/// The run modal's name, which leads its header.
const RUN_LABEL: &str = "Run";

/// One part of the status that ends a Run Block's header, such as its mark or
/// its runtime.
pub(crate) struct StatusPart {
    pub(crate) text: String,
    pub(crate) style: Style,
    /// Whether the part draws as a tag, its text in a box, rather than as
    /// words.
    pub(crate) chip: bool,
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
/// Inside stoatty a rule ends each block, a bar runs beside the body of a
/// failed block and of a Stub, and a rule tops the prompt row, as
/// [`paint_stack_rules`] draws them. A plain terminal draws none of them.
///
/// `terms` holds the sessions of the workspace, which tell whether a Stub's
/// tab still runs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_run_pane(
    run_state: &RunState,
    cmds: &slotmap::SlotMap<CmdId, Cmd>,
    terms: &slotmap::SlotMap<TermId, TermSession>,
    editors: &mut slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    root: &Path,
    home: Option<&Path>,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if area.height < 2 || area.width < 4 {
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
                    let cwd = crate::run::abbreviate_path(cwd, home);
                    let token = command.chars().take_while(|ch| !ch.is_whitespace()).count();
                    let input = cmd_line::token_spans(command, token, theme);
                    let parts = status_parts(block, terms, theme);
                    let header = CmdLine {
                        cwd: &cwd,
                        input: &input,
                        caret: None,
                        status: CmdStatus::Parts(&parts),
                    };
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    cmd_line::cmd_line_row(buf, row, &header, theme, scene);
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

        if !block.finished
            && let Some((done, total)) = block.report.as_ref().and_then(CmdReport::progress)
            && offsets.contains(&1)
        {
            let row = Rect::new(inner.x + 1, row_of(1), inner.width.saturating_sub(1), 1);
            paint_progress(row, done, total, theme, buf, scene);
        }
        if offsets.contains(&(span - 1)) {
            rule_rows.push(row_of(span - 1));
        }
        let body = offsets.start.max(1)..offsets.end;
        let bar = match block.handoff {
            Some(_) => Some(crate::theme::scope::UI_STATUSLINE_RUN),
            None => (block.finished && block.exit_status != Some(0))
                .then_some(crate::theme::scope::UI_ERROR),
        };
        if let Some(scope) = bar
            && !body.is_empty()
        {
            body_bars.push((row_of(body.start), body.len() as u16, scope));
        }
        base += span;
    }

    let abbrev = crate::run::abbreviate_path(root, home);
    let prompt_w = write_prompt(buf, inner.x, input_row, &abbrev, theme);

    let input_area = Rect::new(
        inner.x + prompt_w,
        input_row,
        inner.width.saturating_sub(prompt_w),
        1,
    );
    run_state.input.render(
        editors,
        input_area,
        is_focused,
        "prompt",
        theme,
        chrome,
        &std::collections::BTreeMap::new(),
        buf,
    );

    // The plain terminal has no room for a rule between the rows, so the
    // stack and the prompt stay flush there.
    if scene.live() {
        paint_stack_rules(buf, inner, &rule_rows, &body_bars, input_row, theme, scene);
    }
}

/// Draw the bar beside each body that carries one, the rule under each block,
/// and the rule over the prompt, across `inner`.
///
/// `rule_rows` holds the last row of each block on show, `body_bars` the
/// first row, the row count, and the scope of each body bar on show, and
/// `prompt_row` the prompt's row.
///
/// A block's rule runs along its last row's bottom edge in `ui.run.rule`, and
/// a theme that leaves that unset draws none. A body bar is 4 sixteenths wide
/// at `inner`'s left edge in the foreground of its scope, `ui.error` beside a
/// failed block's body and `ui.statusline.run` beside a Stub's. The prompt's
/// rule runs along the prompt row's top edge in `ui.run.prompt.rule`, or in
/// `ui.divider` for a theme that leaves that unset.
fn paint_stack_rules(
    buf: &mut Buffer,
    inner: Rect,
    rule_rows: &[u16],
    body_bars: &[(u16, u16, &'static str)],
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

    for &(row, count, scope) in body_bars {
        let Some(color) = style_rgb(theme.get(scope).fg) else {
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

/// Draw a running Cmd's progress bar on the top two pixel rows of `row`, the
/// first row of its body, where no glyph reaches.
///
/// The track spans `row` in the `ui.divider` foreground, the color of a
/// meter's track, and the fill covers `done` of `total` of it in the
/// `ui.statusline.run` foreground. A plain terminal draws neither.
pub(super) fn paint_progress(
    row: Rect,
    done: u32,
    total: u32,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if !scene.live() || total == 0 {
        return;
    }
    let width = row.width.saturating_mul(16);
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
            x: 0,
            y: 0,
            width,
            height: 2,
            color,
            radius: 0,
            corners: 0,
        }
        .render(row, buf, scene);
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
/// count after it from the second run on. A Cmd without both a start and an
/// end reads its mark alone. A `cargo test` Cmd whose report counts a failed
/// test reads U+2717, that count, and `failed` as its mark, and a build whose
/// report counts an error reads U+2717 and its count of errors, such as
/// `1 error`.
///
/// A running Cmd reads U+25CF and the whole seconds it has run, as
/// [`Cmd::elapsed_secs`] holds them, in the foreground of
/// `ui.statusline.run`. A running Cmd whose report counts its progress reads
/// the done count over the total between the two, such as `2 / 4`, as
/// [`CmdReport::progress`] gives them. The status-line scope can carry a
/// background, which a header leaves to the pane.
///
/// A Stub reads one part. It is U+25CF and `Terminal tab` in the foreground of
/// `ui.statusline.run` while `terms` holds the session of its tab, and `tab
/// closed` in `ui.text.muted` once the session ended.
pub(crate) fn status_parts(
    cmd: &Cmd,
    terms: &slotmap::SlotMap<TermId, TermSession>,
    theme: &crate::theme::Theme,
) -> Vec<StatusPart> {
    if let Some(handoff) = &cmd.handoff {
        let (text, style) = match terms.contains_key(handoff.term) {
            true => (
                "\u{25cf} Terminal tab",
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

    let kind_chip = cmd.kind.chip().map(|label| StatusPart {
        text: label.to_owned(),
        style: theme
            .try_get(crate::theme::scope::UI_RUN_KIND)
            .unwrap_or_else(|| theme.get(crate::theme::scope::UI_KEY_LABEL)),
        chip: true,
    });
    let source_tag = || {
        let (label, scope) = cmd.source?;
        theme.try_get(scope).map(|style| StatusPart {
            text: label.to_owned(),
            style,
            chip: true,
        })
    };
    let mut parts: Vec<StatusPart> = kind_chip.or_else(source_tag).into_iter().collect();

    if !cmd.finished {
        let fg = theme.get(crate::theme::scope::UI_STATUSLINE_RUN).fg;
        let elapsed = crate::run::format_elapsed(cmd.elapsed_secs);
        let text = match cmd.report.as_ref().and_then(CmdReport::progress) {
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

/// Write the run prompt prefix (abbreviated cwd and the U+276F prompt glyph)
/// at `(x, y)` and return its column width.
///
/// The cwd is styled `UI_KEY_LABEL`. The glyph and whatever the caller writes
/// after it are plain.
fn write_prompt(buf: &mut Buffer, x: u16, y: u16, cwd: &str, theme: &crate::theme::Theme) -> u16 {
    let plain = Style::default();
    let mut col = x;

    write_str(
        buf,
        col,
        y,
        cwd,
        theme.get(crate::theme::scope::UI_KEY_LABEL),
    );
    col += cwd.chars().count() as u16;
    write_str(buf, col, y, " ", plain);
    col += 1;
    // U+276F (heavy right-pointing angle quotation mark) as the prompt glyph.
    write_str(buf, col, y, "\u{276F} ", plain);
    col += 2;
    col - x
}

/// Paint the run modal, the output of `run_state`'s last block, in a box
/// centered over `area`.
///
/// The box is a [`list_plate`](crate::render::chrome::list_plate): its header
/// names the run, one Island holds the output's rows, and the key line notes
/// whether the command still runs or how it ended. With no block, the box
/// holds its header alone. An area under 20 by 8 cells paints nothing.
pub(crate) fn render_modal_run(
    run_state: &RunState,
    cmds: &slotmap::SlotMap<CmdId, Cmd>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if area.width < 20 || area.height < 8 {
        return;
    }

    let box_width = (area.width * 7 / 10).min(area.width.saturating_sub(4));
    let box_height = (area.height * 8 / 10).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_RUN);
    let inner = crate::render::chrome::list_plate(buf, modal_area, modal_style, theme, scene);
    let header = PlateHeader {
        label: RUN_LABEL,
        title: run_state
            .title
            .as_deref()
            .or_else(|| run_state.active_block(cmds).map(|b| b.command.as_str()))
            .unwrap_or_default(),
        meta: &[],
        chip: None,
    };
    crate::render::picker::plate_header(buf, inner, &header, theme, scene);

    let Some(active) = run_state.active_block(cmds) else {
        return;
    };

    let output = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let edges = [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(output, edges, plate, theme, buf, scene);
    }

    // The rows stand a cell inside the Island, 12 px from its edge, as the
    // boards inset a plate's row text.
    let text = Rect::new(
        output.x + 1,
        output.y,
        output.width.saturating_sub(2),
        output.height,
    );
    let grid = &active.grid;
    let visible_rows = text.height as usize;
    let total = grid.line_count();
    let start = total.saturating_sub(visible_rows + run_state.scroll_offset);
    let w = (text.width as usize).min(grid.width() as usize);

    for (i, row_idx) in (start..total).take(visible_rows).enumerate() {
        let y = text.y + i as u16;
        let row = grid.row(row_idx);
        for (col, cell) in row.iter().enumerate().take(w) {
            if cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty()
            {
                continue;
            }
            let mut style = Style::default();
            if let Some(fg) = cell.fg {
                style = style.fg(fg);
            }
            if let Some(bg) = cell.bg {
                style = style.bg(bg);
            }
            style = style.add_modifier(cell.modifiers);
            let cx = text.x + col as u16;
            if cx < text.x + text.width {
                buf[(cx, y)].set_char(cell.ch).set_style(style);
            }
        }
    }

    let status = if active.finished {
        let code = active.exit_status.unwrap_or(-1);
        if code == 0 {
            "done -- press Escape to dismiss".to_owned()
        } else {
            format!("exited {} -- press Escape to dismiss", code)
        }
    } else {
        "running...".to_owned()
    };
    let note = [(
        status.as_str(),
        theme.get(crate::theme::scope::UI_TEXT_MUTED),
    )];
    let footer = Footer {
        note: &note,
        ..Footer::default()
    };
    let footer_rows = crate::render::picker::footer_rect(modal_area);
    crate::render::picker::modal_footer(buf, footer_rows, &footer, plate, theme, scene);
}

#[cfg(test)]
mod tests {
    use super::{
        block_line, paint_grid_row, render_modal_run, render_run_pane, status_parts,
        window_offsets, Body, Buffer, Cmd, GridSelection, Modifier, OutputLine, Rect, RunState,
        StatusPart, Style, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP,
    };
    use crate::{
        render::{editor::ResolvedChrome, paint::style_rgb},
        run::{cargo::ONE_FAILURE, kind},
        test_harness::TestHarness,
        theme::{scope, Theme},
        workspace::Workspace,
    };
    use ratatui::style::Color;
    use std::{
        ops::Range,
        path::PathBuf,
        time::{Duration, Instant},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{self, Command, Edge};

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

    /// Under the plate look the run modal is the list plate's two panels over
    /// its inner cells, then one Island over its output rows, whose text starts
    /// a cell inside it. The header names the run and its command, and the key
    /// line notes how the command ended.
    #[test]
    fn the_run_modal_draws_as_a_list_plate_with_one_island() {
        let (config, _) = stoat_config::parse(
            r##"theme t {
                ui.surface.ground.bg = "#0c0e11";
                ui.background.bg = "#17191e";
                ui.modal.label = { fg: "#c59af0", modifiers: [bold] };
                ui.text.fg = "#dfe2e8";
                ui.text.muted.fg = "#838a98";
            }"##,
        );
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let mut h = TestHarness::with_size(100, 40);
        let executor = h.stoat.executor.clone();
        let mut run = RunState::new(h.stoat.active_workspace_mut(), executor);
        let mut block = block_with("a\r\nb", None);
        block.finished = true;
        block.exit_status = Some(0);
        let mut cmds = slotmap::SlotMap::with_key();
        run.blocks.push(cmds.insert(block));

        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        render_modal_run(&run, &cmds, &theme, area, &mut buf, &mut scene);

        let commands = command::decode_stream(scene.bytes());
        let panels: Vec<_> = commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::Panel(panel) => Some((
                    Rect::new(panel.left, panel.top, panel.width, panel.height),
                    panel.edges,
                    panel.backdrop,
                )),
                _ => None,
            })
            .collect();
        let runs: Vec<&str> = commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::TextRun(run) => Some(run.text.as_str()),
                _ => None,
            })
            .collect();
        let inner = Rect::new(16, 5, 68, 30);
        let plate_edges = [
            Edge::Inset(0),
            Edge::Outset(11),
            Edge::Outset(10),
            Edge::Outset(11),
        ];
        assert_eq!(
            (panels, runs, [buf[(16, 7)].symbol(), buf[(17, 7)].symbol()]),
            (
                vec![
                    (inner, plate_edges, false),
                    (inner, plate_edges, true),
                    (
                        Rect::new(16, 7, 68, 27),
                        [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE],
                        true,
                    ),
                ],
                vec!["Run", "cmd", "done -- press Escape to dismiss"],
                [" ", "a"],
            ),
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
        let chrome = ResolvedChrome::resolve(theme);
        let Workspace {
            runs,
            cmds,
            terms,
            editors,
            git_root,
            ..
        } = h.stoat.active_workspace_mut();
        let run = runs.values().next().expect("an open run");
        let mut buf = Buffer::empty(Rect::new(0, 0, area.right(), area.bottom()));
        let mut scene = ApcScene::new();
        render_run_pane(
            run, cmds, terms, editors, theme, &chrome, git_root, None, area, false, &mut buf,
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
        let mut h = stacked(&[("a", b"one\r\n", Some(0)), ("b", b"", Some(0))]);
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
}
