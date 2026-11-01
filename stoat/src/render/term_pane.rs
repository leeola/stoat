use crate::{
    render::{self, pane, FrameCtx},
    term_session::TermSession,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

/// Composite a term session's emulated screen into `area`.
///
/// The emulator owns a fixed grid the size of `area` (kept in step by
/// [`crate::workspace::Workspace::layout`]), so its origin maps directly onto
/// `area`'s origin. Every cell is painted, including blanks, because the term
/// pane owns the whole rectangle rather than overlaying a shared one. Cells past
/// `area` are clipped so a momentarily-oversized emulator cannot scribble
/// outside its pane.
///
/// When `is_focused` and no selection marks cells, the emulator's cursor cell is
/// drawn as a reversed block, matching how the editor shows its caret only in
/// the focused pane. A drag hides it, so the pane never shows a second cursor
/// beside the selection.
///
/// A mouse selection over the pane
/// ([`TermScreen::selection_range`](crate::term_screen::TermScreen::selection_range))
/// tints its cells with the theme's selection style, so the highlight follows
/// the drag and the text under it, and persists until it is cleared. The
/// zero-width selection a click leaves paints nothing, so a click draws no
/// cell that reads as a cursor.
///
/// See also:
/// - [`render_term_rows`] to paint some rows over an earlier paint.
pub(crate) fn render_term_pane(
    agent: &TermSession,
    theme: &crate::theme::Theme,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
) {
    render_term_rows(
        agent,
        theme,
        area,
        is_focused,
        buf,
        &vec![true; agent.term.rows()],
    );
}

/// Paint the rows of `agent`'s screen that `rows` marks into `area`, as
/// [`render_term_pane`] paints them, and leave every other row of `buf` as it
/// was.
///
/// The cursor's block paints only when `rows` marks the cursor's row. A
/// caller that paints over an earlier paint marks the rows that changed since,
/// which include every row the cursor left or entered.
pub(crate) fn render_term_rows(
    agent: &TermSession,
    theme: &crate::theme::Theme,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
    rows: &[bool],
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let term = &agent.term;
    let row_count = term.rows().min(area.height as usize);
    let cols = term.cols().min(area.width as usize);
    let selection = agent.painted_selection();
    let selection_style = theme.get(crate::theme::scope::UI_SELECTION);
    let marked = |row: usize| rows.get(row).copied().unwrap_or(false);

    let mut cells = Vec::new();
    let mut symbol = String::new();
    for row_idx in (0..row_count).filter(|&row| marked(row)) {
        let y = area.y + row_idx as u16;
        term.row_into(row_idx, &mut cells);
        for (col, cell) in cells.iter().enumerate().take(cols) {
            let x = area.x + col as u16;
            let mut style = Style::default();
            if let Some(fg) = cell.fg {
                style = style.fg(fg);
            }
            if let Some(bg) = cell.bg {
                style = style.bg(bg);
            }
            style = style.add_modifier(cell.modifiers);
            if selection.is_some_and(|range| range.contains(term.grid_point(row_idx, col))) {
                style = style.patch(selection_style);
            }
            let target = &mut buf[(x, y)];
            if cell.marks.is_empty() {
                target.set_char(cell.ch);
            } else {
                symbol.clear();
                symbol.push(cell.ch);
                symbol.extend(&cell.marks);
                target.set_symbol(&symbol);
            }
            target.set_style(style);
        }
    }

    if is_focused
        && selection.is_none()
        && let Some(cursor) = term.cursor()
        && cursor.row < row_count
        && marked(cursor.row)
        && cursor.col < cols
    {
        let x = area.x + cursor.col as u16;
        let y = area.y + cursor.row as u16;
        buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
    }
}

/// Paint again, over a replayed capture of `agent`'s pane in `area`, the rows
/// that `rows` marks, with the fade an unfocused pane takes.
///
/// The cursor's row joins `rows` first, since a focused pane draws its block
/// on every paint. Each row clears to the theme's blank before it paints,
/// because a cell in the terminal's default colors keeps the colors and the
/// modifiers under it, and a whole paint starts from a blank frame. The clear
/// and the fade cover the columns the paint writes and no more, since a cell
/// past the emulator's grid keeps its faded capture.
pub(crate) fn repaint_term_rows(
    agent: &TermSession,
    frame: FrameCtx<'_>,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
    rows: &mut [bool],
) {
    if let Some(cursor) = agent.term.cursor()
        && let Some(row) = rows.get_mut(cursor.row)
    {
        *row = true;
    }
    let rows = &*rows;

    let row_count = agent.term.rows().min(usize::from(area.height));
    let width = agent.term.cols().min(usize::from(area.width)) as u16;
    let lines = || {
        rows.iter()
            .enumerate()
            .take(row_count)
            .filter(|(_, marked)| **marked)
            .map(|(row, _)| Rect {
                y: area.y + row as u16,
                width,
                height: 1,
                ..area
            })
    };

    for line in lines() {
        render::clear_themed(line, buf, frame.theme);
    }
    render_term_rows(agent, frame.theme, area, is_focused, buf, rows);
    for line in lines() {
        pane::dim_unfocused_pane(line, is_focused, frame, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::render_term_pane;
    use crate::{
        pane::{Axis, View},
        test_fixture::fake_term_session,
        theme::Theme,
        Stoat,
    };
    use ratatui::{buffer::Buffer, layout::Rect};

    /// A cell with a combining mark paints its character and the mark as one
    /// symbol, so the accent shows on screen as it does in a copy.
    #[test]
    fn a_combining_mark_paints_with_its_base() {
        let (mut agent, _) = fake_term_session(2, 4);
        agent.term.feed("e\u{301}x".as_bytes());
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);

        render_term_pane(&agent, &Theme::empty(), area, false, &mut buf);

        assert_eq!(
            (buf[(0, 0)].symbol(), buf[(1, 0)].symbol()),
            ("e\u{301}", "x"),
            "the accent shares its letter's cell, and the next cell keeps its own",
        );
    }

    #[test]
    fn snapshot_term_pane_composited_into_split() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(Axis::Vertical);
        let focused = ws.panes.focus();

        let (term, _) = fake_term_session(24, 80);
        let agent_id = ws.terms.insert(term);
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.active_workspace_mut().terms[agent_id]
            .term
            .feed(b"\x1b[1;32mclaude>\x1b[0m ready\r\nsecond line");

        h.assert_snapshot("term_pane_composited_into_split");
    }
}
