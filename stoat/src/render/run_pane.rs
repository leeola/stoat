use crate::{
    render::text::write_str,
    run::{GridSelection, OutputBlock, RunState},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Clear, Widget},
};
use std::{ops::Range, path::Path};

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_run_pane(
    run_state: &RunState,
    editors: &mut slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    home: Option<&Path>,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let input_row = area.y + area.height - 1;
    let output_height = area.height.saturating_sub(1);

    let total = run_state.output_line_total();
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    let window = start..start + visible;

    // Walked block by block rather than flattened first. A block's grid holds
    // its whole scrollback, so materializing the rows would cost the history
    // on every paint to draw a pane's worth of it.
    let mut base = 0;
    for (i, block) in run_state.blocks.iter().enumerate() {
        let prev_exit = i
            .checked_sub(1)
            .and_then(|prev| run_state.blocks[prev].exit_status);
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
                    let row = grid.row(*row_idx);
                    let w = (area.width as usize).min(grid.width() as usize);
                    let row_u16 = u16::try_from(*row_idx).unwrap_or(u16::MAX);
                    for (col, cell) in row.iter().enumerate().take(w) {
                        let col_u16 = u16::try_from(col).unwrap_or(u16::MAX);
                        let selected = selection.is_some_and(|sel| sel.contains(col_u16, row_u16));
                        let blank = cell.ch == ' '
                            && cell.fg.is_none()
                            && cell.bg.is_none()
                            && cell.modifiers.is_empty();
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

    let last_exit = run_state
        .blocks
        .iter()
        .rev()
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
/// one, so this and [`crate::run::OutputBlock::rendered_line_span`] describe the
/// same rows and have to agree. `None` for an offset past them, which the
/// caller's window bounds already exclude.
fn block_line(
    block: &OutputBlock,
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
    GridRow(&'a crate::run::VtermGrid, usize, Option<&'a GridSelection>),
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

pub(crate) fn render_modal_run(
    run_state: &RunState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    if area.width < 20 || area.height < 8 {
        return;
    }

    let box_width = (area.width * 7 / 10).min(area.width.saturating_sub(4));
    let box_height = (area.height * 8 / 10).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let title = {
        let raw = run_state
            .title
            .as_deref()
            .or_else(|| run_state.active_block().map(|b| b.command.as_str()))
            .unwrap_or("run");
        let max = (box_width as usize).saturating_sub(4);
        let display: String = raw.chars().take(max).collect();
        format!(" {display} ")
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_RUN);
    Clear.render(modal_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        modal_area,
        Some(title.as_str()),
        modal_style,
        theme,
        scene,
    );

    let Some(active) = run_state.active_block() else {
        return;
    };

    let grid = &active.grid;
    let visible_rows = (inner.height as usize).saturating_sub(1);
    let total = grid.line_count();
    let start = total.saturating_sub(visible_rows + run_state.scroll_offset);
    let w = (inner.width as usize).min(grid.width() as usize);

    for (i, row_idx) in (start..total).take(visible_rows).enumerate() {
        let y = inner.y + i as u16;
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
            let cx = inner.x + col as u16;
            if cx < inner.x + inner.width {
                buf[(cx, y)].set_char(cell.ch).set_style(style);
            }
        }
    }

    let status_row = inner.y + inner.height.saturating_sub(1);
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
    let status_style = if active.finished {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_BADGE_ACTIVE)
    };
    write_str(buf, inner.x, status_row, &status, status_style);
}

#[cfg(test)]
mod tests {
    use super::{block_line, window_offsets, OutputBlock, OutputLine};
    use std::path::PathBuf;

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

    fn block_with(rows: &str, error: Option<&str>) -> OutputBlock {
        let mut block = OutputBlock::new("cmd".to_owned(), PathBuf::from("/repo"), 20);
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
}
