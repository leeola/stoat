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
pub use vterm::{GridSelection, OutputBlock, StyledCell, TermColor, TermModifier, VtermGrid};

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

    /// Translates a focused-pane-relative `(col, row)` cell into the active
    /// block's `(grid_col, grid_row)`. Returns `None` when the position falls
    /// on the input row, on a non-active block's region, on a header / status
    /// / blank line, on a row scrolled off-screen, or past the active grid's
    /// width. Mirrors the layout that
    /// [`crate::render::run_pane::render_run_pane`] builds.
    pub fn active_block_grid_pos(
        &self,
        area_width: u16,
        area_height: u16,
        col: u16,
        row: u16,
    ) -> Option<(u16, u16)> {
        if area_height < 2 || area_width < 4 {
            return None;
        }
        if col >= area_width || row >= area_height {
            return None;
        }
        let output_height = area_height.saturating_sub(1) as usize;
        if (row as usize) >= output_height {
            return None;
        }

        let active_idx = self.blocks.len().checked_sub(1)?;
        let active = &self.blocks[active_idx];

        let mut idx = 0usize;
        for block in &self.blocks[..active_idx] {
            idx += 1;
            idx += block.grid.line_count();
            if block.error.is_some() {
                idx += 1;
            }
            if block.finished {
                idx += 1;
            }
            idx += 1;
        }
        let active_grid_start = idx + 1;
        let active_grid_end = active_grid_start + active.grid.line_count();

        let mut total = active_grid_end;
        if active.error.is_some() {
            total += 1;
        }
        if active.finished {
            total += 1;
        }
        total += 1;

        let start = total.saturating_sub(output_height + self.scroll_offset);
        let line_idx = start + row as usize;

        if line_idx < active_grid_start || line_idx >= active_grid_end {
            return None;
        }
        if col >= active.grid.width() {
            return None;
        }
        Some((col, (line_idx - active_grid_start) as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty::parse_sentinel_line;

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
    fn grid_scrollback_caps_oldest_rows() {
        let mut grid = VtermGrid::new_with_scrollback(10, 3);
        grid.feed(b"0\r\n1\r\n2\r\n3\r\n4");
        assert_eq!(grid.line_count(), 3);
        assert_eq!(grid.text_in(0..10, 0..3), "2\n3\n4");
    }

    #[test]
    fn grid_under_cap_keeps_all_rows() {
        let mut grid = VtermGrid::new_with_scrollback(10, 5);
        grid.feed(b"a\r\nb\r\nc");
        assert_eq!(grid.line_count(), 3);
        assert_eq!(grid.text_in(0..10, 0..3), "a\nb\nc");
    }

    #[test]
    fn grid_alt_screen_saves_and_restores() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"main");
        grid.feed(b"\x1b[?1049h");
        grid.feed(b"alt");
        assert_eq!(grid.text_in(0..10, 0..1), "alt");
        grid.feed(b"\x1b[?1049l");
        assert_eq!(grid.text_in(0..10, 0..1), "main");
    }

    #[test]
    fn grid_ansi_color_applies() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[31mR\x1b[0mN");
        let row = grid.row(0);
        assert_eq!(row[0].ch, 'R');
        assert_eq!(row[0].fg, Some(TermColor::Red));
        assert_eq!(row[1].ch, 'N');
        assert_eq!(row[1].fg, None);
    }

    #[test]
    fn grid_escape_sequence_spans_feed_calls() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[31");
        grid.feed(b"mR");
        let row = grid.row(0);
        assert_eq!(row[0].ch, 'R');
        assert_eq!(row[0].fg, Some(TermColor::Red));
    }

    #[test]
    fn text_in_extracts_simple_rect() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hello\r\nworld");
        assert_eq!(grid.text_in(0..5, 0..2), "hello\nworld");
    }

    #[test]
    fn text_in_trims_trailing_spaces() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hi");
        assert_eq!(grid.text_in(0..10, 0..1), "hi");
    }

    #[test]
    fn text_in_joins_rows_with_newline() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"a\r\nb\r\nc");
        assert_eq!(grid.text_in(0..1, 0..3), "a\nb\nc");
    }

    #[test]
    fn text_in_clamps_oversize_ranges() {
        let mut grid = VtermGrid::new(5);
        grid.feed(b"abc\r\ndef");
        assert_eq!(grid.text_in(0..100, 0..100), "abc\ndef");
    }

    #[test]
    fn text_in_returns_empty_for_out_of_bounds_rows() {
        let mut grid = VtermGrid::new(5);
        grid.feed(b"abc");
        assert_eq!(grid.text_in(0..5, 5..10), "");
    }

    #[test]
    fn osc52_clipboard_decodes_base64_payload() {
        let mut grid = VtermGrid::new(20);
        // ESC ] 52 ; c ; aGVsbG8= ESC \  -- base64("hello")
        grid.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(grid.clipboard_writes, vec!["hello"]);
    }

    #[test]
    fn osc52_clipboard_accepts_empty_selection_field() {
        let mut grid = VtermGrid::new(20);
        // ESC ] 52 ; ; aGVsbG8= ESC \ -- empty selection means default
        grid.feed(b"\x1b]52;;aGVsbG8=\x1b\\");
        assert_eq!(grid.clipboard_writes, vec!["hello"]);
    }

    #[test]
    fn osc52_drops_primary_only_selection() {
        let mut grid = VtermGrid::new(20);
        // ESC ] 52 ; p ; aGVsbG8= ESC \ -- primary only, no clipboard component
        grid.feed(b"\x1b]52;p;aGVsbG8=\x1b\\");
        assert!(grid.clipboard_writes.is_empty());
    }

    #[test]
    fn osc52_drops_malformed_base64() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]52;c;not_valid_base64!@#\x1b\\");
        assert!(grid.clipboard_writes.is_empty());
    }

    #[test]
    fn osc_other_commands_do_not_write_clipboard() {
        let mut grid = VtermGrid::new(20);
        // OSC 0 (set window title) is a different command
        grid.feed(b"\x1b]0;some title\x1b\\");
        assert!(grid.clipboard_writes.is_empty());
    }

    #[test]
    fn osc52_accepts_clipboard_in_mixed_selection() {
        let mut grid = VtermGrid::new(20);
        // selection "cs" = clipboard + screen, both target system clipboard
        grid.feed(b"\x1b]52;cs;aGVsbG8=\x1b\\");
        assert_eq!(grid.clipboard_writes, vec!["hello"]);
    }

    #[test]
    fn grid_selection_bounds_normalizes_drag_direction() {
        let forward = GridSelection {
            anchor: (3, 1),
            head: (8, 4),
        };
        let reversed = GridSelection {
            anchor: (8, 4),
            head: (3, 1),
        };
        assert_eq!(forward.bounds(), reversed.bounds());
        assert_eq!(forward.bounds(), ((3, 1), (8, 4)));
    }

    #[test]
    fn grid_selection_bounds_swaps_columns_when_rows_match() {
        let sel = GridSelection {
            anchor: (10, 2),
            head: (3, 2),
        };
        assert_eq!(sel.bounds(), ((3, 2), (10, 2)));
    }

    #[test]
    fn grid_selection_contains_single_row() {
        let sel = GridSelection {
            anchor: (3, 1),
            head: (8, 1),
        };
        assert!(!sel.contains(2, 1));
        assert!(sel.contains(3, 1));
        assert!(sel.contains(5, 1));
        assert!(sel.contains(8, 1));
        assert!(!sel.contains(9, 1));
        assert!(!sel.contains(5, 0));
        assert!(!sel.contains(5, 2));
    }

    #[test]
    fn grid_selection_contains_multi_row() {
        let sel = GridSelection {
            anchor: (5, 1),
            head: (3, 3),
        };
        assert!(!sel.contains(4, 1));
        assert!(sel.contains(5, 1));
        assert!(sel.contains(99, 1));
        assert!(sel.contains(0, 2));
        assert!(sel.contains(99, 2));
        assert!(sel.contains(0, 3));
        assert!(sel.contains(3, 3));
        assert!(!sel.contains(4, 3));
        assert!(!sel.contains(0, 4));
    }

    #[test]
    fn text_for_selection_single_row() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hello world");
        let sel = GridSelection {
            anchor: (1, 0),
            head: (3, 0),
        };
        assert_eq!(grid.text_for_selection(&sel), "ell");
    }

    #[test]
    fn text_for_selection_multi_row_joins_with_newline() {
        let mut grid = VtermGrid::new(8);
        grid.feed(b"foo\r\nbar\r\nbaz");
        let sel = GridSelection {
            anchor: (1, 0),
            head: (1, 2),
        };
        assert_eq!(grid.text_for_selection(&sel), "oo\nbar\nba");
    }

    #[test]
    fn text_for_selection_normalises_reverse_drag() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"abcdef");
        let forward = GridSelection {
            anchor: (1, 0),
            head: (4, 0),
        };
        let reverse = GridSelection {
            anchor: (4, 0),
            head: (1, 0),
        };
        assert_eq!(grid.text_for_selection(&forward), "bcde");
        assert_eq!(grid.text_for_selection(&reverse), "bcde");
    }

    #[test]
    fn text_for_selection_out_of_bounds_returns_empty() {
        let mut grid = VtermGrid::new(5);
        grid.feed(b"hi");
        let sel = GridSelection {
            anchor: (0, 5),
            head: (4, 7),
        };
        assert_eq!(grid.text_for_selection(&sel), "");
    }

    #[test]
    fn text_for_selection_three_rows_keeps_middle() {
        let mut grid = VtermGrid::new(6);
        grid.feed(b"alpha\r\nbeta\r\ngamma");
        let sel = GridSelection {
            anchor: (2, 0),
            head: (2, 2),
        };
        assert_eq!(grid.text_for_selection(&sel), "pha\nbeta\ngam");
    }
}
