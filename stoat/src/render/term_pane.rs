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
/// When `is_focused`, the emulator's cursor cell is drawn as a reversed block,
/// matching how the editor shows its caret only in the focused pane.
pub(crate) fn render_term_pane(
    agent: &TermSession,
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

    for row_idx in 0..rows {
        let y = area.y + row_idx as u16;
        let cells = term.row(row_idx);
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
            buf[(x, y)].set_char(cell.ch).set_style(style);
        }
    }

    if is_focused
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
    use crate::{
        host::{FakeTerminalSession, TerminalSession},
        pane::{Axis, View},
        term_screen::TermScreen,
        term_session::TermSession,
        Stoat,
    };
    use std::sync::Arc;

    #[test]
    fn snapshot_term_pane_composited_into_split() {
        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(Axis::Vertical);
        let focused = ws.panes.focus();

        let session: Arc<dyn TerminalSession> = Arc::new(FakeTerminalSession::new());
        let agent_id = ws
            .terms
            .insert(TermSession::new(TermScreen::new(24, 80), session));
        ws.panes.pane_mut(focused).view = View::Agent(agent_id);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.active_workspace_mut().terms[agent_id]
            .term
            .feed(b"\x1b[1;32mclaude>\x1b[0m ready\r\nsecond line");

        h.assert_snapshot("term_pane_composited_into_split");
    }
}
