use crate::{
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
        cmd_line::{self, CmdLine, CmdStatus},
        paint::style_rgb,
        picker::{Footer, PlateHeader},
        text::write_str,
    },
    run::{Cmd, CmdId, GridSelection, RunState, StyledCell, VtermGrid},
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

/// Paint a run pane's output blocks and its prompt row into `area`.
///
/// The prompt row takes the pane's last row. Inside stoatty a hairline in
/// `ui.divider` runs along the row's top edge and rules it off the output.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_run_pane(
    run_state: &RunState,
    cmds: &slotmap::SlotMap<CmdId, Cmd>,
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

    let input_row = area.y + area.height - 1;
    let output_height = area.height.saturating_sub(1);

    let total = run_state.output_line_total(cmds);
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    let window = start..start + visible;

    // Walked block by block rather than flattened first. A block's grid holds
    // its whole scrollback, so materializing the rows would cost the history
    // on every paint to draw a pane's worth of it.
    let mut base = 0;
    for block in run_state.blocks.iter().filter_map(|id| cmds.get(*id)) {
        let grid_rows = block.grid.rendered_line_count();
        let span = block.rendered_line_span();

        for offset in window_offsets(base, span, &window) {
            let y = area.y + (base + offset - start) as u16;
            let Some(line) = block_line(block, grid_rows, offset) else {
                continue;
            };
            match &line {
                OutputLine::Prompt { cwd, command } => {
                    let cwd = crate::run::abbreviate_path(cwd, home);
                    let token = command.chars().take_while(|ch| !ch.is_whitespace()).count();
                    let input = cmd_line::token_spans(command, token, theme);
                    let parts = status_parts(block, theme);
                    let header = CmdLine {
                        cwd: &cwd,
                        input: &input,
                        caret: None,
                        status: CmdStatus::Parts(&parts),
                    };
                    let row = Rect::new(area.x, y, area.width, 1);
                    cmd_line::cmd_line_row(buf, row, &header, theme, scene);
                },
                OutputLine::GridRow(grid, row_idx, selection) => {
                    paint_grid_row(buf, area, y, grid, *row_idx, *selection);
                },
                OutputLine::Error(msg) => {
                    let max_w = area.width as usize;
                    let display: String = msg.chars().take(max_w).collect();
                    write_str(
                        buf,
                        area.x,
                        y,
                        &display,
                        theme.get(crate::theme::scope::UI_ERROR),
                    );
                },
            }
        }

        base += span;
    }

    let abbrev = crate::run::abbreviate_path(root, home);
    let prompt_w = write_prompt(buf, area.x, input_row, &abbrev, theme);

    let input_area = Rect::new(
        area.x + prompt_w,
        input_row,
        area.width.saturating_sub(prompt_w),
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

    // The plain terminal has no room for a rule between the output and the
    // prompt, so the row stays flush there.
    if let Some(color) =
        style_rgb(theme.get(crate::theme::scope::UI_DIVIDER).fg).filter(|_| scene.live())
    {
        Bar {
            x: 0,
            y: 0,
            width: area.width.saturating_mul(16),
            height: 1,
            color,
            radius: 0,
            corners: 0,
        }
        .render(Rect::new(area.x, input_row, area.width, 1), buf, scene);
    }
}

/// The status that ends `cmd`'s header, as the parts it reads in order.
///
/// A finished Cmd reads its mark, U+2713 in `ui.badge.complete` for an exit of
/// 0, and otherwise U+2717 in `ui.error`, with `exit N` after it when the
/// process gave a code. Its runtime follows in `ui.text.muted`, with the run
/// count after it from the second run on. A Cmd without both a start and an
/// end reads its mark alone.
///
/// A running Cmd reads U+25CF and the whole seconds it has run, as
/// [`Cmd::elapsed_secs`] holds them, in the foreground of
/// `ui.statusline.run`. The status-line scope can carry a background, which a
/// header leaves to the pane.
pub(crate) fn status_parts(cmd: &Cmd, theme: &crate::theme::Theme) -> Vec<StatusPart> {
    if !cmd.finished {
        let fg = theme.get(crate::theme::scope::UI_STATUSLINE_RUN).fg;
        return vec![StatusPart {
            text: format!("\u{25cf} {}", crate::run::format_elapsed(cmd.elapsed_secs)),
            style: Style {
                fg,
                ..Style::default()
            },
            chip: false,
        }];
    }

    let (mark, mark_scope) = match cmd.exit_status {
        Some(0) => (
            "\u{2713}".to_owned(),
            crate::theme::scope::UI_BADGE_COMPLETE,
        ),
        Some(code) => (
            format!("\u{2717} exit {code}"),
            crate::theme::scope::UI_ERROR,
        ),
        None => ("\u{2717}".to_owned(), crate::theme::scope::UI_ERROR),
    };
    let mut parts = vec![StatusPart {
        text: mark,
        style: theme.get(mark_scope),
        chip: false,
    }];

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
fn paint_grid_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    grid: &VtermGrid,
    row_idx: usize,
    selection: Option<&GridSelection>,
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
        if let Some(fg) = cell.fg {
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

/// The line drawn at `offset` within a block's span, where `grid_rows` is the
/// block's rendered grid height.
///
/// The span is the prompt, then the grid, then an error line if the block has
/// one, so this and [`crate::run::Cmd::rendered_line_span`] describe the
/// same rows and have to agree. `None` for an offset past them, which the
/// caller's window bounds already exclude.
fn block_line(block: &Cmd, grid_rows: usize, offset: usize) -> Option<OutputLine<'_>> {
    match offset.checked_sub(1) {
        None => Some(OutputLine::Prompt {
            cwd: &block.cwd,
            command: block.command.as_str(),
        }),
        Some(row) if row < grid_rows => Some(OutputLine::GridRow(
            &block.grid,
            row,
            block.selection.as_ref(),
        )),
        Some(_) => block.error.as_deref().map(OutputLine::Error),
    }
}

enum OutputLine<'a> {
    Prompt { cwd: &'a Path, command: &'a str },
    GridRow(&'a VtermGrid, usize, Option<&'a GridSelection>),
    Error(&'a str),
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
        block_line, paint_grid_row, render_modal_run, status_parts, window_offsets, Buffer, Cmd,
        GridSelection, Modifier, OutputLine, Rect, RunState, Style, ISLAND_BOTTOM, ISLAND_SIDE,
        ISLAND_TOP,
    };
    use crate::{
        render::paint::style_rgb,
        test_harness::TestHarness,
        theme::{scope, Theme},
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
            .map(|offset| tag(block_line(&block, grid_rows, offset)))
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
            .map(|offset| tag(block_line(&block, grid_rows, offset)))
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
        paint_grid_row(&mut buf, area, 0, &block.grid, 0, Some(&selection));

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

    /// Inside stoatty a hairline in the divider color rules the prompt row off
    /// the output above it, along the row's top edge and across the pane.
    #[test]
    fn a_divider_hairline_tops_the_prompt_row() {
        let mut h = TestHarness::with_size(60, 12);
        h.open_run();
        h.render_composited();

        let content = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let divider =
            style_rgb(h.stoat.theme.get(scope::UI_DIVIDER).fg).expect("an RGB divider color");
        let hairlines: Vec<_> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) if bar.height == 1 && bar.color == divider => {
                    Some((bar.x, bar.y, bar.width))
                },
                _ => None,
            })
            .collect();

        let prompt_row = content.y + content.height - 1;
        assert_eq!(
            hairlines,
            [(
                (content.x * 16) as i16,
                (prompt_row * 16) as i16,
                content.width * 16
            )],
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
        status_parts(cmd, theme)
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
    /// pane's right edge.
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

        let (row, right) = ((area.y * 16) as i16, (area.right() * 16) as i16);
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

    /// The header of `echo ok` draws `echo` as the command token and ` ok` as
    /// plain text, after the cwd `/w` and the glyph.
    #[test]
    fn a_header_colors_its_first_word() {
        let mut h = TestHarness::with_size(60, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/w");
        h.open_run();
        h.submit_run("echo ok");
        let buf = h.render_composited();

        let area = pane_area(&h);
        let theme = &h.stoat.theme;
        let looks = |cols: Range<u16>| {
            cols.map(|col| {
                let cell = &buf[(area.x + col, area.y)];
                (cell.fg, cell.modifier)
            })
            .collect::<Vec<_>>()
        };
        let look = |style: Style, cells: usize| {
            vec![(style.fg.unwrap_or(Color::Reset), style.add_modifier); cells]
        };
        assert_eq!(
            (looks(5..9), looks(9..12)),
            (
                look(theme.get(scope::UI_CMD_COMMAND), 4),
                look(theme.get(scope::UI_TEXT), 3)
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
}
