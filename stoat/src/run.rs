pub mod pty;
pub mod vterm;

use crate::{
    input_view::{InputView, SubmitTarget},
    workspace::Workspace,
};
pub use pty::{spawn_oneshot, spawn_shell, PtyNotification, ShellHandle};
use slotmap::new_key_type;
use std::path::PathBuf;
use stoat_scheduler::Executor;
pub use vterm::{OutputBlock, StyledCell, VtermGrid};

new_key_type! {
    pub struct RunId;
}

pub struct RunState {
    pub(crate) input: InputView,
    pub blocks: Vec<OutputBlock>,
    pub scroll_offset: usize,
    pub cwd: PathBuf,
    pub shell_handle: Option<ShellHandle>,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub title: Option<String>,
}

impl RunState {
    /// Construct a new run state with an empty [`InputView`] for the command
    /// prompt. The actual [`RunId`] is resolved from pane focus at submit
    /// time, so construction does not need the key yet.
    pub fn new(cwd: PathBuf, ws: &mut Workspace, executor: Executor) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::Run, "", "prompt", 1);
        Self {
            input,
            blocks: Vec::new(),
            scroll_offset: 0,
            cwd,
            shell_handle: None,
            history: Vec::new(),
            history_cursor: None,
            title: None,
        }
    }

    pub fn active_block(&self) -> Option<&OutputBlock> {
        self.blocks.last()
    }

    pub fn active_block_mut(&mut self) -> Option<&mut OutputBlock> {
        self.blocks.last_mut()
    }

    pub fn is_running(&self) -> bool {
        self.blocks.last().is_some_and(|b| !b.finished)
    }

    pub fn history_up(&mut self, ws: &mut Workspace) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_cursor {
            Some(i) if i > 0 => i - 1,
            Some(_) => return,
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(idx);
        let entry = self.history[idx].clone();
        self.input.replace_text(ws, &entry);
    }

    pub fn history_down(&mut self, ws: &mut Workspace) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.history.len() {
            self.history_cursor = Some(idx + 1);
            let entry = self.history[idx + 1].clone();
            self.input.replace_text(ws, &entry);
        } else {
            self.history_cursor = None;
            self.input.replace_text(ws, "");
        }
    }

    /// Remove the run's [`InputView`] scratch editor. Called on pane close to
    /// avoid leaking editor slots.
    pub fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty::parse_sentinel_line;
    use ratatui::style::Color;

    #[test]
    fn parse_sentinel_valid() {
        assert_eq!(parse_sentinel_line("__STOAT_5__ 0"), Some(0));
        assert_eq!(parse_sentinel_line("__STOAT_5__ 127"), Some(127));
    }

    #[test]
    fn parse_sentinel_invalid() {
        assert_eq!(parse_sentinel_line("hello"), None);
        assert_eq!(parse_sentinel_line("__STOAT_5__"), None);
        assert_eq!(parse_sentinel_line("__STOAT_5__ abc"), None);
    }

    #[test]
    fn grid_default_empty() {
        let grid = VtermGrid::new(80);
        assert_eq!(grid.width(), 80);
        assert_eq!(grid.line_count(), 1);
    }

    #[test]
    fn grid_writes_ascii() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hello");
        let row = grid.row(0);
        let expected: Vec<char> = row.iter().map(|c| c.ch).collect();
        let prefix: String = expected.iter().take(5).collect();
        assert_eq!(prefix, "hello");
    }

    #[test]
    fn grid_newline_advances_row() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"ab\r\ncd");
        assert!(grid.line_count() >= 2);
        let row0: String = grid.row(0).iter().take(2).map(|c| c.ch).collect();
        let row1: String = grid.row(1).iter().take(2).map(|c| c.ch).collect();
        assert_eq!(row0, "ab");
        assert_eq!(row1, "cd");
    }

    #[test]
    fn grid_ansi_color_applies() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[31mR\x1b[0mN");
        let row = grid.row(0);
        assert_eq!(row[0].ch, 'R');
        assert_eq!(row[0].fg, Some(Color::Red));
        assert_eq!(row[1].ch, 'N');
        assert_eq!(row[1].fg, None);
    }
}
