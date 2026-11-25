use crate::{
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
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
    let mut prev_exit = None;
    for block in run_state.blocks.iter().filter_map(|id| cmds.get(*id)) {
        let grid_rows = block.grid.rendered_line_count();
        let span = block.rendered_line_span();

        for offset in window_offsets(base, span, &window) {
            let y = area.y + (base + offset - start) as u16;
            let Some(line) = block_line(block, grid_rows, offset, prev_exit) else {
                continue;
            };
            match &line {
                OutputLine::Prompt {
                    cwd,
                    prev_exit,
                    command,
                } => {
                    let abbrev = crate::run::abbreviate_path(cwd, home);
                    let pw = write_prompt(buf, area.x, y, &abbrev, *prev_exit, theme);
                    let max_w = (area.width as usize).saturating_sub(pw as usize);
                    let display: String = command.chars().take(max_w).collect();
                    write_str(buf, area.x + pw, y, &display, Style::default());
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
        prev_exit = block.exit_status;
    }

    let last_exit = run_state
        .blocks
        .iter()
        .rev()
        .filter_map(|id| cmds.get(*id))
        .find(|block| block.finished)
        .and_then(|block| block.exit_status);
    let abbrev = crate::run::abbreviate_path(&run_state.cwd, home);
    let prompt_w = write_prompt(buf, area.x, input_row, &abbrev, last_exit, theme);

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
fn block_line(
    block: &Cmd,
    grid_rows: usize,
    offset: usize,
    prev_exit: Option<i32>,
) -> Option<OutputLine<'_>> {
    match offset.checked_sub(1) {
        None => Some(OutputLine::Prompt {
            cwd: &block.cwd,
            prev_exit,
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
    Prompt {
        cwd: &'a Path,
        prev_exit: Option<i32>,
        command: &'a str,
    },
    GridRow(&'a VtermGrid, usize, Option<&'a GridSelection>),
    Error(&'a str),
}

/// Write the run prompt prefix (abbreviated cwd, optional exit flag, and the
/// U+276F prompt glyph) at `(x, y)` and return its column width.
///
/// The cwd is styled `UI_KEY_LABEL`. The `[N]` exit flag appears only when
/// `exit_flag` is a nonzero code, styled `UI_ERROR`. The glyph and whatever
/// the caller writes after it are plain.
fn write_prompt(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    cwd: &str,
    exit_flag: Option<i32>,
    theme: &crate::theme::Theme,
) -> u16 {
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
    if let Some(code) = exit_flag.filter(|&c| c != 0) {
        let flag = format!("[{code}]");
        write_str(buf, col, y, &flag, theme.get(crate::theme::scope::UI_ERROR));
        col += flag.chars().count() as u16;
    }
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
        block_line, paint_grid_row, render_modal_run, window_offsets, Buffer, Cmd, GridSelection,
        Modifier, OutputLine, Rect, RunState, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP,
    };
    use crate::{
        render::paint::style_rgb,
        test_harness::TestHarness,
        theme::{scope, Theme},
    };
    use std::path::PathBuf;
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
            .map(|offset| tag(block_line(&block, grid_rows, offset, None)))
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
            .map(|offset| tag(block_line(&block, grid_rows, offset, None)))
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
        let mut run = RunState::new(
            PathBuf::from("/repo"),
            h.stoat.active_workspace_mut(),
            executor,
        );
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
}
