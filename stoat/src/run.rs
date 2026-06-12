pub mod key_encode;
pub mod pty;
pub mod term;
pub mod vterm;

use crate::{
    input_view::{InputView, SubmitTarget},
    workspace::Workspace,
};
pub use key_encode::encode_key;
pub use pty::{spawn_shell, PtyNotification, ShellHandle};
use slotmap::new_key_type;
use std::{path::PathBuf, time::Instant};
use stoat_scheduler::Executor;
pub use vterm::{
    encode_mouse_report, BlockStatus, CommandMark, CursorShape, GridSelection, LinkTarget,
    MouseProtocol, OutputBlock, StyledCell, TermColor, TermModifier, TerminalLink, VtermGrid,
};

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
    /// Whether the active block's command has begun executing, set by the
    /// OSC 133 `C` mark. Gates [`Self::apply_command_marks`] so the shell's
    /// startup `D` mark -- emitted before the first command runs -- does not
    /// finish the block before its command has started.
    command_started: bool,
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
            command_started: false,
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

    /// Apply OSC 133 command marks drained from the active block's grid. A
    /// `Start` (`C`) mark records that the command's output has begun; a
    /// `Done` (`D`) mark then finishes the active block with its exit code
    /// and clears the started flag. Gating on `Start` keeps the shell's
    /// startup `D` mark -- emitted before the first command runs -- from
    /// finishing the block prematurely.
    pub(crate) fn apply_command_marks(&mut self, marks: &[CommandMark], now: Instant) {
        for mark in marks {
            match mark {
                CommandMark::Start => self.command_started = true,
                CommandMark::Done { exit } => {
                    if self.command_started {
                        self.command_started = false;
                        if let Some(block) = self.blocks.last_mut() {
                            block.finish(*exit, now);
                        }
                    }
                },
            }
        }
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
            idx += 1;
            idx += 1;
        }
        let active_grid_start = idx + 1;
        let active_grid_end = active_grid_start + active.grid.line_count();

        let mut total = active_grid_end;
        if active.error.is_some() {
            total += 1;
        }
        total += 1;
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
    use std::time::Duration;

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
    fn grid_is_alt_screen_tracks_enter_and_leave() {
        let mut grid = VtermGrid::new(10);
        assert!(!grid.is_alt_screen());
        grid.feed(b"\x1b[?1049h");
        assert!(grid.is_alt_screen());
        grid.feed(b"\x1b[?1049l");
        assert!(!grid.is_alt_screen());
    }

    #[test]
    fn grid_resize_widens_and_pads_rows() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hi");
        grid.resize(20);
        assert_eq!(grid.width(), 20);
        assert_eq!(grid.row(0).len(), 20);
        assert_eq!(grid.text_in(0..20, 0..1), "hi");
    }

    #[test]
    fn grid_resize_narrows_and_truncates_rows() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"0123456789");
        grid.resize(5);
        assert_eq!(grid.width(), 5);
        assert_eq!(grid.row(0).len(), 5);
        assert_eq!(grid.text_in(0..5, 0..1), "01234");
    }

    #[test]
    fn grid_resize_clamps_cursor_into_bounds() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[1;10H");
        grid.resize(5);
        assert_eq!(grid.cursor_position(), (0, 5));
    }

    #[test]
    fn grid_resize_on_alt_screen_resizes_saved_main() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"main");
        grid.feed(b"\x1b[?1049h");
        grid.resize(20);
        grid.feed(b"\x1b[?1049l");
        assert_eq!(grid.width(), 20);
        assert_eq!(grid.row(0).len(), 20);
        assert_eq!(grid.text_in(0..20, 0..1), "main");
    }

    #[test]
    fn grid_resize_is_noop_when_unchanged_or_zero() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"hi");
        grid.resize(10);
        grid.resize(0);
        assert_eq!(grid.width(), 10);
        assert_eq!(grid.row(0).len(), 10);
        assert_eq!(grid.text_in(0..10, 0..1), "hi");
    }

    #[test]
    fn grid_cup_positions_absolutely() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[2;3HX");
        assert_eq!(grid.row(1)[2].ch, 'X');
    }

    #[test]
    fn grid_save_and_restore_cursor() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[3;4H\x1b[s\x1b[1;1H\x1b[uZ");
        assert_eq!(grid.row(2)[3].ch, 'Z');
    }

    #[test]
    fn grid_scroll_region_scrolls_within_margins() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"A\r\nB\r\nC\r\nD");
        grid.feed(b"\x1b[2;4r\x1b[4;1H\nE");
        assert_eq!(grid.text_in(0..10, 0..4), "A\nC\nD\nE");
    }

    #[test]
    fn grid_tracks_mouse_protocol() {
        let mut grid = VtermGrid::new(10);
        assert_eq!(grid.mouse_protocol(), MouseProtocol::None);
        grid.feed(b"\x1b[?1002h");
        assert_eq!(grid.mouse_protocol(), MouseProtocol::ButtonEvent);
        grid.feed(b"\x1b[?1002l");
        assert_eq!(grid.mouse_protocol(), MouseProtocol::None);
    }

    #[test]
    fn grid_encodes_mouse_sgr_and_x10() {
        let mut grid = VtermGrid::new(10);
        assert_eq!(
            grid.encode_mouse(0, 0, 4, 2, true),
            Some(vec![0x1b, b'[', b'M', 32, 37, 35])
        );
        grid.feed(b"\x1b[?1006h");
        assert_eq!(
            grid.encode_mouse(0, 0, 4, 2, true),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            grid.encode_mouse(0, 0, 4, 2, false),
            Some(b"\x1b[<0;5;3m".to_vec())
        );
    }

    #[test]
    fn grid_x10_release_is_button_3_and_clamps_range() {
        let grid = VtermGrid::new(10);
        assert_eq!(
            grid.encode_mouse(2, 0, 1, 1, false),
            Some(vec![0x1b, b'[', b'M', 35, 34, 34])
        );
        assert_eq!(grid.encode_mouse(0, 0, 300, 0, true), None);
    }

    #[test]
    fn grid_decscusr_selects_cursor_shape() {
        let mut grid = VtermGrid::new(10);
        assert_eq!(grid.cursor_shape(), CursorShape::Block);
        grid.feed(b"\x1b[5 q");
        assert_eq!(grid.cursor_shape(), CursorShape::Bar);
        grid.feed(b"\x1b[3 q");
        assert_eq!(grid.cursor_shape(), CursorShape::Underline);
        grid.feed(b"\x1b[2 q");
        assert_eq!(grid.cursor_shape(), CursorShape::Block);
    }

    #[test]
    fn grid_cursor_position_tracks_writes() {
        let mut grid = VtermGrid::new(10);
        assert_eq!(grid.cursor_position(), (0, 0));
        grid.feed(b"ab\r\nc");
        assert_eq!(grid.cursor_position(), (1, 1));
    }

    #[test]
    fn grid_search_finds_literal_matches() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"hello world\r\nhello again");
        let matches = grid.search("hello");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].bounds(), ((0, 0), (4, 0)));
        assert_eq!(matches[1].bounds(), ((0, 1), (4, 1)));
    }

    #[test]
    fn grid_search_regex_and_empty() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"abc123def");
        let matches = grid.search(r"\d+");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].bounds(), ((3, 0), (5, 0)));
        assert!(grid.search("zzz").is_empty());
        assert!(grid.search("[bad").is_empty());
    }

    #[test]
    fn grid_links_detect_url_and_path() {
        let mut grid = VtermGrid::new(40);
        grid.feed(b"see https://x.io and src/x.rs:12:3 ok");
        let links = grid.links();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, LinkTarget::Url("https://x.io".to_string()));
        assert_eq!(links[0].selection.bounds().0, (4, 0));
        assert_eq!(
            links[1].target,
            LinkTarget::Path {
                path: "src/x.rs".to_string(),
                line: Some(12),
                column: Some(3),
            }
        );
        assert_eq!(links[1].selection.bounds().0, (21, 0));
    }

    #[test]
    fn grid_links_ignore_bare_number_colon() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"error 42:10 here");
        assert!(grid.links().is_empty());
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
    fn osc7_sets_cwd_from_file_uri() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]7;file://host/Users/lee/projects\x07");
        assert_eq!(grid.cwd(), Some("/Users/lee/projects"));
    }

    #[test]
    fn osc7_percent_decodes_the_path() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]7;file://host/tmp/a%20b\x07");
        assert_eq!(grid.cwd(), Some("/tmp/a b"));
    }

    #[test]
    fn osc7_ignores_non_file_uri() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]7;http://host/path\x07");
        assert_eq!(grid.cwd(), None);
    }

    #[test]
    fn osc133_c_marks_command_start() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]133;C\x07");
        assert_eq!(grid.command_marks, vec![CommandMark::Start]);
    }

    #[test]
    fn osc133_d_marks_command_done_with_exit() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]133;D;0\x07");
        assert_eq!(
            grid.command_marks,
            vec![CommandMark::Done { exit: Some(0) }]
        );
    }

    #[test]
    fn osc133_d_without_exit_has_no_code() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]133;D\x07");
        assert_eq!(grid.command_marks, vec![CommandMark::Done { exit: None }]);
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

    #[test]
    fn block_status_reflects_finished_and_exit() {
        let mut block = OutputBlock::new("cmd".into(), 10, Instant::now(), PathBuf::from("/w"));
        assert_eq!(block.status(), BlockStatus::Running);
        block.finished = true;
        block.exit_status = Some(0);
        assert_eq!(block.status(), BlockStatus::Succeeded);
        block.exit_status = Some(2);
        assert_eq!(block.status(), BlockStatus::Failed(Some(2)));
        block.exit_status = None;
        assert_eq!(block.status(), BlockStatus::Failed(None));
    }

    #[test]
    fn block_records_cwd_and_derives_duration_on_finish() {
        let start = Instant::now();
        let mut block = OutputBlock::new("cmd".into(), 10, start, PathBuf::from("/work"));
        assert_eq!(block.cwd, PathBuf::from("/work"));
        assert_eq!(block.duration(), None);

        block.finish(Some(0), start + Duration::from_millis(1500));
        assert_eq!(block.ended_at, Some(start + Duration::from_millis(1500)));
        assert_eq!(block.duration(), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn block_header_meta_formats_cwd_and_duration() {
        let start = Instant::now();
        let mut block = OutputBlock::new("ls".into(), 10, start, PathBuf::from("/work"));
        assert_eq!(block.duration_label(), None);
        assert_eq!(block.header_meta(), "/work");

        block.finish(Some(0), start + Duration::from_millis(250));
        assert_eq!(block.duration_label(), Some("250ms".to_string()));
        assert_eq!(block.header_meta(), "/work  250ms");

        let mut secs = OutputBlock::new("x".into(), 10, start, PathBuf::from("/w"));
        secs.finish(Some(0), start + Duration::from_millis(1500));
        assert_eq!(secs.duration_label(), Some("1.5s".to_string()));
    }

    #[test]
    fn block_status_labels() {
        assert_eq!(BlockStatus::Running.label(), "[running]");
        assert_eq!(BlockStatus::Succeeded.label(), "[exit 0]");
        assert_eq!(BlockStatus::Failed(Some(2)).label(), "[exit 2]");
        assert_eq!(BlockStatus::Failed(None).label(), "[exit ?]");
    }
}
