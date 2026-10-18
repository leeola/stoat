use crate::term_session::TermSession;
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
/// A mouse selection over the pane ([`TermSession::selection`]) tints its cells
/// with the theme's selection style so the highlight follows the drag and
/// persists until it is cleared. The zero-width selection a click leaves paints
/// nothing, so a click draws no cell that reads as a cursor.
pub(crate) fn render_term_pane(
    agent: &TermSession,
    theme: &crate::theme::Theme,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let term = &agent.term;
    let rows = term.rows().min(area.height as usize);
    let cols = term.cols().min(area.width as usize);
    let selection = agent.painted_selection();
    let selection_style = theme.get(crate::theme::scope::UI_SELECTION);

    let mut cells = Vec::new();
    let mut symbol = String::new();
    for row_idx in 0..rows {
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
            if selection.is_some_and(|sel| sel.contains(row_idx, col)) {
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
        && cursor.row < rows
        && cursor.col < cols
    {
        let x = area.x + cursor.col as u16;
        let y = area.y + cursor.row as u16;
        buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
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
