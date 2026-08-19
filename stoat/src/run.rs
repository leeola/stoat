pub mod pty;
pub mod vterm;

use crate::{
    input_history::InputHistory,
    input_view::{InputView, SubmitTarget},
    workspace::Workspace,
};
pub use pty::{
    agent_socket_path, spawn_claude, spawn_oneshot, spawn_shell, spawn_term_reader, spawn_terminal,
    PtyNotification, ShellHandle,
};
use ratatui::layout::Rect;
use slotmap::new_key_type;
use std::path::{Component, Path, PathBuf};
use stoat_scheduler::Executor;
pub use vterm::{CommandMark, GridSelection, OutputBlock, StyledCell, VtermGrid};

new_key_type! {
    pub struct RunId;
}

/// Output rows one wheel notch moves the run pane's scrollback.
const WHEEL_STEP_ROWS: usize = 3;

pub struct RunState {
    pub(crate) input: InputView,
    pub blocks: Vec<OutputBlock>,
    pub scroll_offset: usize,
    pub cwd: PathBuf,
    pub shell_handle: Option<ShellHandle>,
    /// Fish-style recall history of commands run in this pane, walked by
    /// Up/Down in the prompt.
    ///
    /// Session-scoped rather than persisted. A shell line is bound to the
    /// state the pane was in when it ran, so it is worth recalling within a
    /// session and misleading across a restart.
    pub(crate) history: InputHistory,
    pub title: Option<String>,
}

impl RunState {
    /// Construct a new run state with an empty [`InputView`] for the command
    /// prompt. The actual [`RunId`] is resolved from pane focus at submit
    /// time, so construction does not need the key yet.
    pub fn new(cwd: PathBuf, ws: &mut Workspace, executor: Executor) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::Run, "", "insert", 1);
        Self {
            input,
            blocks: Vec::new(),
            scroll_offset: 0,
            cwd,
            shell_handle: None,
            history: InputHistory::default(),
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

    /// Recall the previous command against the needle `current`, or [`None`] to
    /// leave the prompt unchanged.
    ///
    /// Returns the line rather than writing it, because the prompt's
    /// [`InputView`] writes through the workspace this state lives in.
    pub(crate) fn history_prev(&mut self, current: &str) -> Option<String> {
        self.history.reset_if_edited(current);
        self.history.prev(current)
    }

    /// Recall the next command toward the newest, restoring the
    /// originally-typed text past the newest match.
    pub(crate) fn history_next(&mut self, current: &str) -> Option<String> {
        self.history.reset_if_edited(current);
        self.history.next(current)
    }

    /// Remove the run's [`InputView`] scratch editor. Called on pane close to
    /// avoid leaking editor slots.
    pub fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    /// Move the output scroll by one wheel step, for a pane whose output area
    /// is `page` rows tall.
    ///
    /// Clamped on the way down as well as up. Trimming a block's scrollback
    /// shrinks the range under an offset already held, and without the clamp a
    /// scroll down would spend its steps walking a stale offset back into range
    /// rather than moving the view.
    pub fn wheel_scroll(&mut self, down: bool, page: usize) {
        let max = self.output_line_total().saturating_sub(page);
        self.scroll_offset = if down {
            self.scroll_offset.min(max).saturating_sub(WHEEL_STEP_ROWS)
        } else {
            (self.scroll_offset + WHEEL_STEP_ROWS).min(max)
        };
    }

    /// Total run-pane output rows across every block, matching what the
    /// renderer draws.
    ///
    /// The wheel-scroll clamp and [`Self::active_block_grid_pos`] both size the
    /// output by this, so the drawn rows, the scrollable range, and the
    /// mouse-hit rows stay in lockstep.
    pub fn output_line_total(&self) -> usize {
        self.blocks
            .iter()
            .map(OutputBlock::rendered_line_span)
            .sum()
    }

    /// Translates a focused-pane-relative `(col, row)` cell into the active
    /// block's `(grid_col, grid_row)`. Returns `None` when the position falls
    /// on the input row, on a non-active block's region, on a header / status
    /// / blank line, on a row scrolled off-screen, or past the active grid's
    /// width. Mirrors the layout that
    /// [`crate::render::run_pane::render_run_pane`] builds.
    pub fn active_block_grid_pos(&self, area: Rect, col: u16, row: u16) -> Option<(u16, u16)> {
        if area.height < 2 || area.width < 4 {
            return None;
        }
        if col >= area.width || row >= area.height {
            return None;
        }
        let output_height = area.height.saturating_sub(1) as usize;
        if (row as usize) >= output_height {
            return None;
        }

        let active_idx = self.blocks.len().checked_sub(1)?;
        let active = &self.blocks[active_idx];

        let idx: usize = self.blocks[..active_idx]
            .iter()
            .map(OutputBlock::rendered_line_span)
            .sum();
        let active_grid_start = idx + 1;
        let active_grid_end = active_grid_start + active.grid.rendered_line_count();

        let total = self.output_line_total();

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

/// Abbreviate `path` for a prompt in the style of fish's `prompt_pwd`.
///
/// A `home` prefix collapses to `~`. Every path component except the last is
/// shortened to its first character, keeping a leading dot (`.config` ->
/// `.c`); the final component is kept whole. A non-`home` absolute path keeps
/// its leading slash (`/usr/local/bin` -> `/u/l/bin`).
pub(crate) fn abbreviate_path(path: &Path, home: Option<&Path>) -> String {
    let (tilde, rest) = match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => (true, rest),
        None => (false, path),
    };

    let mut leading_slash = false;
    let mut names: Vec<String> = Vec::new();
    for comp in rest.components() {
        match comp {
            Component::RootDir => leading_slash = true,
            Component::Normal(s) => names.push(s.to_string_lossy().into_owned()),
            _ => {},
        }
    }

    let last = names.len().saturating_sub(1);
    let body = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == last {
                name.clone()
            } else {
                abbreviate_component(name)
            }
        })
        .collect::<Vec<_>>()
        .join("/");

    match (tilde, leading_slash) {
        (true, _) if body.is_empty() => "~".to_string(),
        (true, _) => format!("~/{body}"),
        (false, true) => format!("/{body}"),
        (false, false) => body,
    }
}

/// Shorten one path component to its first character, preserving a leading
/// dot so hidden directories stay recognizable (`.config` -> `.c`).
fn abbreviate_component(name: &str) -> String {
    match name.strip_prefix('.') {
        Some(rest) => match rest.chars().next() {
            Some(c) => format!(".{c}"),
            None => ".".to_string(),
        },
        None => name.chars().next().map(String::from).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::BufferId, editor_state::EditorId};
    use ratatui::style::Color;

    #[test]
    fn abbreviate_path_shortens_ancestors_under_home() {
        let home = PathBuf::from("/Users/lee");
        assert_eq!(
            abbreviate_path(&PathBuf::from("/Users/lee/projects/stoat"), Some(&home)),
            "~/p/stoat",
        );
        assert_eq!(
            abbreviate_path(&PathBuf::from("/Users/lee/.config/foo"), Some(&home)),
            "~/.c/foo",
        );
        assert_eq!(abbreviate_path(&home, Some(&home)), "~");
    }

    #[test]
    fn abbreviate_path_without_home_keeps_absolute_root() {
        assert_eq!(
            abbreviate_path(&PathBuf::from("/usr/local/bin"), None),
            "/u/l/bin"
        );
        assert_eq!(abbreviate_path(&PathBuf::from("/tmp"), None), "/tmp");
        assert_eq!(abbreviate_path(&PathBuf::from("/"), None), "/");
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

    #[test]
    fn grid_escape_sequence_spans_feed_calls() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"\x1b[31");
        grid.feed(b"mR");
        let row = grid.row(0);
        assert_eq!(row[0].ch, 'R');
        assert_eq!(row[0].fg, Some(Color::Red));
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
    fn osc133_records_start_and_done_with_exit() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]133;C\x07");
        grid.feed(b"\x1b]133;D;0\x07");
        assert_eq!(
            grid.command_marks,
            vec![CommandMark::Start, CommandMark::Done { exit: Some(0) }],
        );
    }

    #[test]
    fn osc133_done_without_exit_is_none() {
        let mut grid = VtermGrid::new(20);
        // ST-terminated (ESC \) with no exit field.
        grid.feed(b"\x1b]133;D\x1b\\");
        assert_eq!(grid.command_marks, vec![CommandMark::Done { exit: None }]);
    }

    #[test]
    fn osc7_records_cwd_bare_and_with_host() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]7;file:///Users/lee\x07");
        grid.feed(b"\x1b]7;file://myhost/tmp\x07");
        assert_eq!(
            grid.cwd_reports,
            vec![PathBuf::from("/Users/lee"), PathBuf::from("/tmp")],
        );
    }

    #[test]
    fn osc7_ignores_non_file_uri() {
        let mut grid = VtermGrid::new(20);
        grid.feed(b"\x1b]7;http://example.com/x\x07");
        assert!(grid.cwd_reports.is_empty());
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

    /// Well past the cap, so a trim has certainly run.
    fn flood() -> String {
        "x\r\n".repeat(vterm::MAX_SCROLLBACK_ROWS * 2)
    }

    #[test]
    fn a_flood_of_output_stops_growing_at_the_scrollback_cap() {
        let mut grid = VtermGrid::new(10);
        grid.feed(flood().as_bytes());
        assert!(
            grid.line_count() <= vterm::MAX_SCROLLBACK_ROWS + vterm::SCROLLBACK_SLACK,
            "{} rows held past a {}-row cap",
            grid.line_count(),
            vterm::MAX_SCROLLBACK_ROWS
        );
    }

    #[test]
    fn the_cursor_still_writes_the_newest_row_after_a_trim() {
        let mut grid = VtermGrid::new(10);
        grid.feed(flood().as_bytes());
        grid.feed(b"tail");

        let last = grid.line_count() - 1;
        let row: String = grid.row(last).iter().take(4).map(|c| c.ch).collect();
        assert_eq!(row, "tail", "output after a trim lands on the last row");
    }

    #[test]
    fn feed_reports_the_rows_it_dropped() {
        let mut grid = VtermGrid::new(10);
        let trimmed = grid.feed(flood().as_bytes());

        assert!(trimmed > 0, "a flood past the cap drops rows");
        assert_eq!(grid.feed(b"more"), 0, "a feed inside the cap drops none");
    }

    #[test]
    fn command_marks_survive_a_trim() {
        let mut grid = VtermGrid::new(10);
        grid.feed(flood().as_bytes());
        grid.feed(b"\x1b]133;D;7\x07");

        assert_eq!(
            grid.command_marks,
            vec![CommandMark::Done { exit: Some(7) }],
            "marks carry no row index, so trimming rows cannot lose them"
        );
    }

    #[test]
    fn a_selection_moves_down_with_the_rows_that_scrolled_off() {
        let sel = GridSelection {
            anchor: (2, 30),
            head: (5, 40),
        };
        assert_eq!(
            sel.shifted_up(10),
            Some(GridSelection {
                anchor: (2, 20),
                head: (5, 30)
            }),
            "both endpoints move by what fell off, columns untouched"
        );
    }

    #[test]
    fn a_selection_survives_a_trim_only_while_some_of_it_remains() {
        let sel = GridSelection {
            anchor: (2, 3),
            head: (5, 8),
        };
        assert_eq!(
            [sel.shifted_up(9), sel.shifted_up(5)],
            [
                None,
                Some(GridSelection {
                    anchor: (2, 0),
                    head: (5, 3)
                })
            ],
            "gone when every row fell off, clamped to the top when only some did"
        );
    }

    /// A run state holding `blocks` one-line blocks, without a workspace behind
    /// it. Only the scroll arithmetic reads these.
    fn scrollable(blocks: usize, scroll_offset: usize) -> RunState {
        let mut run = RunState {
            input: InputView {
                editor_id: EditorId::default(),
                buffer_id: BufferId::new(0),
                target: SubmitTarget::Run,
                max_height: 1,
            },
            blocks: Vec::new(),
            scroll_offset,
            cwd: PathBuf::from("/repo"),
            shell_handle: None,
            history: InputHistory::default(),
            title: None,
        };
        for n in 0..blocks {
            let mut block = OutputBlock::new(format!("cmd{n}"), PathBuf::from("/repo"), 10);
            block.feed(b"out");
            run.blocks.push(block);
        }
        run
    }

    #[test]
    fn a_wheel_walks_the_scrollback_and_stops_at_its_ends() {
        let mut run = scrollable(20, 0);
        let page = 5;

        run.wheel_scroll(false, page);
        run.wheel_scroll(false, page);
        assert_eq!(run.scroll_offset, 6, "two notches walk back six rows");

        for _ in 0..20 {
            run.wheel_scroll(false, page);
        }
        assert_eq!(
            run.scroll_offset,
            run.output_line_total() - page,
            "scrolling back stops with a page still shown"
        );
    }

    #[test]
    fn a_wheel_down_over_a_shrunken_scrollback_moves_the_view_at_once() {
        let mut run = scrollable(4, 5_000);
        let page = 5;
        let max = run.output_line_total() - page;

        run.wheel_scroll(true, page);

        assert_eq!(
            run.scroll_offset,
            max.saturating_sub(3),
            "an offset left over from a longer scrollback moves on the first notch"
        );
    }

    #[test]
    fn a_blocks_selection_follows_its_grid_through_a_trim() {
        let mut block = OutputBlock::new("cmd".to_owned(), PathBuf::from("/repo"), 10);
        block.selection = Some(GridSelection {
            anchor: (0, 0),
            head: (0, 1),
        });
        block.feed(flood().as_bytes());

        assert_eq!(
            block.selection, None,
            "a selection on rows long since scrolled off is dropped"
        );
    }
}

#[cfg(test)]
mod history_tests {
    use crate::{pane::View, test_harness::TestHarness, Stoat};

    /// The run prompt's text, for asserting what a recall wrote into it.
    fn prompt_text(h: &TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let View::Run(id) = ws.panes.pane(focused).view else {
            panic!("the focused pane is not a run pane");
        };
        ws.runs.get(id).expect("run state").input.text(ws)
    }

    /// A run pane with `commands` already submitted in it.
    fn ran(commands: &[&str]) -> TestHarness {
        let mut h = Stoat::test();
        h.open_run();
        for command in commands {
            h.submit_run(command);
        }
        h
    }

    /// Up walks back through the commands run in this pane, newest first, and
    /// Down walks toward the newest again.
    #[test]
    fn up_and_down_walk_the_commands_this_pane_ran() {
        let mut h = ran(&["ls", "cargo build"]);

        h.type_keys("up");
        assert_eq!(
            prompt_text(&h),
            "cargo build",
            "the newest comes back first"
        );
        h.type_keys("up");
        assert_eq!(prompt_text(&h), "ls", "and the one before it next");
        h.type_keys("down");
        assert_eq!(
            prompt_text(&h),
            "cargo build",
            "Down walks toward the newest"
        );
        h.type_keys("down");
        assert_eq!(
            prompt_text(&h),
            "",
            "and past the newest restores the empty prompt"
        );
    }

    /// Running the same command twice keeps one entry, so a repeated build
    /// does not push the rest of the history out of reach.
    #[test]
    fn a_repeated_command_collapses_to_one_entry() {
        let mut h = ran(&["ls", "cargo build", "cargo build"]);

        h.type_keys("up");
        assert_eq!(prompt_text(&h), "cargo build");
        h.type_keys("up");
        assert_eq!(
            prompt_text(&h),
            "ls",
            "one step past the build reaches what came before it"
        );
    }

    /// The typed text is the needle, so a prefix narrows the walk to the
    /// commands that carry it.
    #[test]
    fn the_typed_text_is_the_needle_the_walk_filters_on() {
        let mut h = ran(&["cargo build", "ls"]);

        h.type_text("car");
        h.type_keys("up");
        assert_eq!(
            prompt_text(&h),
            "cargo build",
            "the needle skips the newer command it does not match"
        );
    }

    /// Editing during a walk ends it, so the next Up captures the edited text
    /// as a fresh needle rather than continuing the old walk.
    #[test]
    fn typing_after_a_recall_recaptures_the_needle() {
        let mut h = ran(&["alpha", "beta"]);

        h.type_keys("up");
        assert_eq!(prompt_text(&h), "beta", "the walk starts at the newest");

        h.type_text("x");
        h.type_keys("up");
        assert_eq!(
            prompt_text(&h),
            "betax",
            "the edit ends the walk, and the new needle matches nothing"
        );
    }
}
