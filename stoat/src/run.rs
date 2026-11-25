pub(crate) mod cargo;
pub(crate) mod kind;
pub mod pty;
pub mod vterm;

use crate::{
    input_view::{InputView, SubmitTarget},
    term_session::{TermId, TermSession},
    workspace::Workspace,
};
use cargo::CmdReport;
pub use pty::{
    agent_socket_bind_dir, agent_socket_dir, agent_socket_path, agent_socket_path_in, spawn_claude,
    spawn_ssh_reader, spawn_term_reader, spawn_terminal, PtyNotification, TermSpawnEnv,
};
use ratatui::layout::Rect;
use slotmap::{new_key_type, SlotMap};
use std::{
    path::{Component, Path},
    time::Duration,
};
use stoat_scheduler::Executor;
use vterm::{BodyRow, Tone};
pub use vterm::{Cmd, GridSelection, StyledCell, VtermGrid};

new_key_type! {
    pub struct RunId;
    /// The key of a [`Cmd`] in [`Workspace::cmds`].
    ///
    /// A place that shows a Cmd names it by this key, so several places can
    /// show one Cmd.
    pub struct CmdId;
}

/// Output rows one wheel notch moves the run pane's scrollback.
const WHEEL_STEP_ROWS: usize = 3;

/// Rows of output a run pane keeps across all its blocks before it drops the
/// oldest block.
///
/// It equals one block's scrollback. A pane that runs many commands then holds
/// about what a terminal emulator retains, not every row of every run.
const MAX_RUN_ROWS: usize = vterm::MAX_SCROLLBACK_ROWS;

pub struct RunState {
    pub(crate) input: InputView,
    /// The stack, oldest first. Each id names a Cmd in `Workspace::cmds`.
    pub blocks: Vec<CmdId>,
    pub scroll_offset: usize,
    /// What the prompt shows for the text it holds, which the frame resolves
    /// once for each change of the text.
    pub(crate) prompt: PromptInfo,
    /// The line that a walk of the workspace's command history wrote into the
    /// prompt, while that walk lasts.
    ///
    /// The prompt names a recall, and Up and Down walk on, while its text is
    /// this line, so an edit drops the name and starts a fresh walk. A walk
    /// from another prompt clears it, because one walk runs at a time.
    pub(crate) recalled: Option<String>,
}

/// The command token and the tag at its right that the Run pane prompt shows
/// for its text.
///
/// The palette's rule for a head reads the command set and the context of
/// the workspace, which the painter does not hold. So the frame resolves the
/// prompt once for each change of its text, and the painter reads this.
#[derive(Default, Hash)]
pub(crate) struct PromptInfo {
    /// The text that the other fields were resolved for.
    pub(crate) text: String,
    /// The chars of the text, from its start, that the command token takes.
    pub(crate) token_len: usize,
    /// The Kind Chip that the text's Run Kind gives.
    pub(crate) chip: Option<&'static str>,
    /// The label and the scope of the Source Tag of the command that the
    /// text's head names.
    pub(crate) source: Option<(&'static str, &'static str)>,
    /// Whether the text is a line that the active walk of the command history
    /// recalled, which the prompt names at its right.
    pub(crate) recalled: bool,
}

impl RunState {
    /// Construct a new run state with an empty [`InputView`] for the command
    /// prompt. The actual [`RunId`] is resolved from pane focus at submit
    /// time, so construction does not need the key yet.
    pub fn new(ws: &mut Workspace, executor: Executor) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::Run, "", "insert");
        Self {
            input,
            blocks: Vec::new(),
            scroll_offset: 0,
            prompt: PromptInfo::default(),
            recalled: None,
        }
    }

    /// The run's blocks as plain text, for a preview pane.
    ///
    /// Each block reads as its `$ <command>` line, then the output rows the
    /// run pane shows for it. A blank line separates the blocks.
    pub(crate) fn preview_text(&self, cmds: &SlotMap<CmdId, Cmd>) -> String {
        let mut text = String::new();
        for block in self.blocks.iter().filter_map(|id| cmds.get(*id)) {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("$ ");
            text.push_str(&block.command);

            let grid = &block.grid;
            let output = grid.text_in(0..grid.width() as usize, 0..grid.rendered_line_count());
            if !output.is_empty() {
                text.push('\n');
                text.push_str(&output);
            }
        }
        text
    }

    /// The newest Cmd of the stack, the one that output arrives into.
    pub fn active_block<'a>(&self, cmds: &'a SlotMap<CmdId, Cmd>) -> Option<&'a Cmd> {
        self.blocks.last().and_then(|id| cmds.get(*id))
    }

    /// The newest Cmd of the stack, for a write.
    ///
    /// It borrows the store alone, so the caller can write the run state while
    /// it holds the Cmd.
    pub fn active_block_mut<'a>(&self, cmds: &'a mut SlotMap<CmdId, Cmd>) -> Option<&'a mut Cmd> {
        self.blocks.last().and_then(|id| cmds.get_mut(*id))
    }

    /// Drop the oldest finished blocks while the pane holds more than one
    /// scrollback of rows, and remove each dropped Cmd from `cmds`.
    ///
    /// Each block caps its own scrollback. The block count has no cap, so a
    /// pane that runs a flooding command many times keeps every row of every
    /// run until this drops them.
    ///
    /// The threshold is [`MAX_RUN_ROWS`] plus [`vterm::SCROLLBACK_SLACK`], the
    /// most rows one block holds. The newest block always stays, and so does
    /// each Cmd that still runs, so a flood in one Cmd kills no other
    /// process. A Cmd in the Job Tray stays too, because the tray carries it.
    /// An id with no Cmd counts for no rows.
    pub(crate) fn trim_blocks(&mut self, cmds: &mut SlotMap<CmdId, Cmd>) {
        let mut rows: usize = self
            .blocks
            .iter()
            .filter_map(|id| cmds.get(*id))
            .map(|block| block.grid.line_count())
            .sum();
        while rows > MAX_RUN_ROWS + vterm::SCROLLBACK_SLACK {
            let newest = self.blocks.len().saturating_sub(1);
            let Some(oldest) = self.blocks[..newest]
                .iter()
                .position(|id| cmds.get(*id).is_none_or(|cmd| cmd.finished && !cmd.in_jobs))
            else {
                return;
            };
            let dropped = self.blocks.remove(oldest);
            rows -= cmds
                .remove(dropped)
                .map_or(0, |block| block.grid.line_count());
        }
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
    pub fn wheel_scroll(&mut self, down: bool, page: usize, cmds: &SlotMap<CmdId, Cmd>) {
        let max = self.output_line_total(cmds).saturating_sub(page);
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
    /// mouse-hit rows stay in lockstep. An id with no Cmd counts for no rows,
    /// and the [`Self::hint`] row counts for one.
    pub fn output_line_total(&self, cmds: &SlotMap<CmdId, Cmd>) -> usize {
        let spans: usize = self
            .blocks
            .iter()
            .filter_map(|id| cmds.get(*id))
            .map(Cmd::rendered_line_span)
            .sum();
        spans + usize::from(self.has_hint(cmds))
    }

    /// The hint row after the newest block's last row, which names the keys
    /// that act on the block.
    ///
    /// `Some` when the newest Cmd finished and its report located a test
    /// failure or a build error, as [`CmdReport::first_location`] reads it.
    /// That row names the keys that run the block again and that go to the
    /// first of them.
    ///
    /// A newest Stub's row names `Enter`, which goes to its tab, while `terms`
    /// holds the tab's session, and `C-r`, which runs it again, once the
    /// session ended. The row counts as the block's last row, so the block's
    /// rule runs under it and a block's bar runs beside it.
    ///
    /// A newest block of matches, a Cmd with [`Cmd::finished_locations`],
    /// names `Enter`, which opens the first match, and `C-q`, which opens the
    /// matches as a location list.
    ///
    /// The row of a newest Stub of a Job, a Cmd [`Cmd::in_jobs`], comes before
    /// each other rule. It names `C-j`, which brings the Job back to its block.
    pub(crate) fn hint(
        &self,
        cmds: &SlotMap<CmdId, Cmd>,
        terms: &SlotMap<TermId, TermSession>,
    ) -> Option<BodyRow> {
        if self.active_block(cmds).is_some_and(|cmd| cmd.in_jobs) {
            return Some(BodyRow {
                spans: vec![
                    ("C-j".to_owned(), Tone::Key),
                    (" bring it back here".to_owned(), Tone::Muted),
                ],
            });
        }
        if let Some(handoff) = self.active_block(cmds).and_then(|cmd| cmd.handoff.as_ref()) {
            let (key, action) = match terms.contains_key(handoff.term) {
                true => ("Enter", " go to tab"),
                false => ("C-r", " run again"),
            };
            return Some(BodyRow {
                spans: vec![
                    (key.to_owned(), Tone::Key),
                    (action.to_owned(), Tone::Muted),
                ],
            });
        }
        if self
            .active_block(cmds)
            .is_some_and(|cmd| cmd.finished_locations().is_some())
        {
            return Some(BodyRow {
                spans: vec![
                    ("Enter".to_owned(), Tone::Key),
                    (" open location \u{b7} ".to_owned(), Tone::Muted),
                    ("C-q".to_owned(), Tone::Key),
                    (" to location list".to_owned(), Tone::Muted),
                ],
            });
        }
        self.has_hint(cmds).then(|| BodyRow {
            spans: vec![
                ("C-r".to_owned(), Tone::Key),
                (" run again \u{b7} ".to_owned(), Tone::Muted),
                ("Enter".to_owned(), Tone::Key),
                (" go to failure".to_owned(), Tone::Muted),
            ],
        })
    }

    /// Whether the newest block carries the [`Self::hint`] row, which a Stub
    /// always does.
    fn has_hint(&self, cmds: &SlotMap<CmdId, Cmd>) -> bool {
        self.active_block(cmds).is_some_and(|cmd| {
            cmd.in_jobs
                || cmd.handoff.is_some()
                || cmd.finished_locations().is_some()
                || cmd
                    .finished_report()
                    .and_then(CmdReport::first_location)
                    .is_some()
        })
    }

    /// Translates a focused-pane-relative `(col, row)` cell into the active
    /// block's `(grid_col, grid_row)`. Returns `None` when the position falls
    /// on the input row, on the blank rows over a short stack, on a non-active
    /// block's region, on a header / status / blank line, on a row scrolled
    /// off-screen, left of the body's column, or past the active grid's width.
    /// An active block that shows its report draws no grid row, so every
    /// position over it answers `None`, and so does every position over a
    /// Stub, which draws its one row in place of its grid. Mirrors the layout
    /// that [`crate::render::run_pane::render_run_pane`] builds.
    pub fn active_block_grid_pos(
        &self,
        cmds: &SlotMap<CmdId, Cmd>,
        area: Rect,
        col: u16,
        row: u16,
    ) -> Option<(u16, u16)> {
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
        let active = cmds.get(self.blocks[active_idx])?;
        if active.report_rows().is_some() {
            return None;
        }

        let idx: usize = self.blocks[..active_idx]
            .iter()
            .filter_map(|id| cmds.get(*id))
            .map(Cmd::rendered_line_span)
            .sum();
        let active_grid_start = idx + 1;
        let active_grid_end = active_grid_start + active.grid.rendered_line_count();

        let total = self.output_line_total(cmds);

        let start = total.saturating_sub(output_height + self.scroll_offset);
        let pad = output_height.saturating_sub(total);
        let line_idx = start + (row as usize).checked_sub(pad)?;

        if line_idx < active_grid_start || line_idx >= active_grid_end {
            return None;
        }
        let body_col = stack_rect(area).x - area.x + 1;
        let grid_col = col.checked_sub(body_col)?;
        if grid_col >= active.grid.width() {
            return None;
        }
        Some((grid_col, (line_idx - active_grid_start) as u16))
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

/// The runtime `elapsed` as a Run Block's status reads it, in the unit that
/// its size takes.
///
/// Under a second it reads whole milliseconds (`18ms`), under a minute
/// seconds with one decimal digit (`1.2s`), under an hour `m:ss` (`2:31`), and
/// from an hour `h:mm:ss` (`1:02:05`). The decimal digit is cut rather than
/// rounded, so a runtime never reads as the next unit's first value, as
/// 59.99 s would read `60.0s`.
pub(crate) fn format_runtime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0 => format!("{}ms", elapsed.as_millis()),
        1..60 => {
            let tenths = elapsed.as_millis() / 100;
            format!("{}.{}s", tenths / 10, tenths % 10)
        },
        _ => format_elapsed(secs),
    }
}

/// The time a running Cmd has run, `secs` whole seconds, as its status reads
/// it.
///
/// It reads `m:ss` from zero (`0:00`, `2:31`) and `h:mm:ss` from an hour
/// (`1:02:05`). A running Cmd's status counts whole seconds, so it has no
/// millisecond form.
pub(crate) fn format_elapsed(secs: u64) -> String {
    match secs {
        0..3600 => format!("{}:{:02}", secs / 60, secs % 60),
        _ => format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60),
    }
}

/// The rect that a Run pane's stack and prompt draw in, one column inside
/// each side of the pane's `area`, or `area` whole under 6 columns.
///
/// The board pads its island 12 px, and one 9 px cell puts the text 10 px
/// from the island's edge. A pane too narrow to spare the two columns keeps
/// them for its text.
pub(crate) fn stack_rect(area: Rect) -> Rect {
    if area.width < 6 {
        return area;
    }
    Rect::new(area.x + 1, area.y, area.width - 2, area.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::BufferId, editor_state::EditorId, run::cargo::ONE_FAILURE};
    use ratatui::style::Color;
    use std::{collections::BTreeSet, path::PathBuf};

    #[test]
    fn runtime_reads_in_the_unit_its_size_takes() {
        let millis = [
            0, 18, 999, 1_000, 1_249, 59_999, 60_000, 151_000, 3_600_000, 3_725_000,
        ];
        assert_eq!(
            millis.map(|ms| format_runtime(Duration::from_millis(ms))),
            [
                "0ms", "18ms", "999ms", "1.0s", "1.2s", "59.9s", "1:00", "2:31", "1:00:00",
                "1:02:05"
            ],
        );
    }

    #[test]
    fn elapsed_reads_minutes_and_seconds() {
        assert_eq!(
            [0, 21, 151, 3_725].map(format_elapsed),
            ["0:00", "0:21", "2:31", "1:02:05"],
        );
    }

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

    /// Every cell `row` holds, untrimmed, so a row padded past its content
    /// reads longer than the content.
    fn row_text(grid: &VtermGrid, row: usize) -> String {
        grid.row(row).iter().map(|cell| cell.ch).collect()
    }

    /// The parser hands DEL to print, where a terminal ignores it.
    #[test]
    fn a_delete_byte_takes_no_cell() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"a\x7fb");
        assert_eq!(row_text(&grid, 0), "ab");
    }

    /// A character that does not fit wraps to the next row, but only once it
    /// comes, so a line that fills its row and ends with CRLF stays one row.
    #[test]
    fn a_long_line_wraps_at_the_width() {
        let mut grid = VtermGrid::new(5);
        grid.feed(b"abcdefg\r\nhijkl\r\nm");
        assert_eq!(grid.text_in(0..5, 0..4), "abcde\nfg\nhijkl\nm");
    }

    /// A wide character takes two cells, and one that does not fit in what is
    /// left of the row wraps whole.
    #[test]
    fn a_wide_character_takes_two_cells() {
        let mut grid = VtermGrid::new(5);
        grid.feed("\u{4e2d}\u{6587}\u{4e2d}".as_bytes());
        assert_eq!(grid.text_in(0..5, 0..2), "\u{4e2d}\u{6587}\n\u{4e2d}");
    }

    #[test]
    fn a_finished_row_holds_only_its_cells() {
        let mut grid = VtermGrid::new(200);
        assert_eq!(grid.row(0).len(), 0, "a fresh grid's row");

        grid.feed(b"hello   \r\nhi  \x1b[2B");
        let held: Vec<(usize, usize)> = (0..3)
            .map(|row| (grid.row(row).len(), grid.row_capacity(row)))
            .collect();
        assert_eq!(
            (held, grid.row(3).len()),
            (vec![(5, 5), (2, 2), (0, 0)], 0),
            "rows the cursor leaves or skips hold their written cells and no room past them"
        );
    }

    #[test]
    fn a_cursor_up_rewrites_a_shrunk_row() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"abc\r\n\x1b[Awxyz");
        assert_eq!(
            row_text(&grid, 0),
            "wxyz",
            "the rewrite runs past where the finished row was cut"
        );
    }

    #[test]
    fn erasing_to_the_line_end_cuts_the_row_at_the_cursor() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"abcdef\r\x1b[3C\x1b[K");
        assert_eq!(row_text(&grid, 0), "abc");
    }

    #[test]
    fn erasing_below_the_cursor_cuts_its_row_and_clears_the_rest() {
        let mut grid = VtermGrid::new(10);
        grid.feed(b"ab\r\ncd\r\nef\x1b[2A\r\x1b[1C\x1b[J");
        let rows: Vec<String> = (0..3).map(|row| row_text(&grid, row)).collect();
        assert_eq!(rows, ["a", "", ""]);
    }

    #[test]
    fn erasing_before_a_cursor_past_the_rows_end_blanks_its_cells() {
        for (erase, expected) in [("1K", ["ab", "  "]), ("1J", ["", "  "])] {
            let mut grid = VtermGrid::new(10);
            grid.feed(format!("ab\r\ncd\x1b[5C\x1b[{erase}").as_bytes());
            let rows: Vec<String> = (0..2).map(|row| row_text(&grid, row)).collect();
            assert_eq!(rows, expected, "CSI {erase}");
        }
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
    fn osc52_accepts_clipboard_in_mixed_selection() {
        let mut grid = VtermGrid::new(20);
        // selection "cs" = clipboard + screen, both target system clipboard
        grid.feed(b"\x1b]52;cs;aGVsbG8=\x1b\\");
        assert_eq!(grid.clipboard_writes, vec!["hello"]);
    }

    /// The block's parser buffers an OSC payload without bound and keeps the
    /// capacity for the block's life, so an oversized one is cut before it
    /// arrives. The cut ends where the escape does, or the escapes and the
    /// output behind it would go with it.
    #[test]
    fn an_oversize_osc_leaves_the_escape_behind_it_alone() {
        let mut grid = VtermGrid::new(20);

        let mut seq = b"\x1b]0;".to_vec();
        seq.resize(seq.len() + 1024 * 1024, b'a');
        seq.extend_from_slice(b"\x07\x1b]52;c;aGVsbG8=\x07done");
        grid.feed(&seq);

        assert_eq!(
            (grid.clipboard_writes.clone(), grid.text_in(0..4, 0..1)),
            (vec!["hello".to_owned()], "done".to_owned()),
        );
    }

    /// A clipboard write is the one code with a reason to carry a large
    /// payload, so the cap that bounds a title lets a whole selection through.
    #[test]
    fn a_clipboard_write_past_the_plain_cap_still_stores() {
        let mut grid = VtermGrid::new(20);

        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend_from_slice("QUFB".repeat(256 * 1024).as_bytes());
        seq.push(0x07);
        grid.feed(&seq);

        assert_eq!(grid.clipboard_writes, vec!["A".repeat(3 * 256 * 1024)]);
    }

    /// A clipboard write that trips its cap a read after it started leaves the
    /// parser holding the base64 of the reads before. Any terminator makes the
    /// parser dispatch that, and a length in fours decodes to a cut clipboard.
    #[test]
    fn a_clipboard_write_split_past_its_cap_stores_nothing() {
        let mut grid = VtermGrid::with_osc_caps(20, 16, 64);

        grid.feed(format!("\x1b]52;c;{}", "QUFB".repeat(8)).as_bytes());
        grid.feed(format!("{}\x07", "QUFB".repeat(10)).as_bytes());
        grid.feed(b"after");

        assert_eq!(grid.clipboard_writes, Vec::<String>::new());
        assert_eq!(grid.text_in(0..5, 0..1), "after");
    }

    /// A clipboard write past its cap within one read leaves the parser too
    /// few arguments to store.
    #[test]
    fn a_clipboard_write_past_its_cap_stores_nothing() {
        let mut grid = VtermGrid::with_osc_caps(20, 16, 64);

        grid.feed(format!("\x1b]52;c;{}\x07after", "QUFB".repeat(20)).as_bytes());

        assert_eq!(grid.clipboard_writes, Vec::<String>::new());
        assert_eq!(grid.text_in(0..5, 0..1), "after");
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

    /// A run state holding `blocks` one-line blocks, with the store of their
    /// Cmds and without a workspace behind it. Each Cmd but the newest has
    /// finished. Only the scroll and trim arithmetic reads these.
    fn scrollable(blocks: usize, scroll_offset: usize) -> (RunState, SlotMap<CmdId, Cmd>) {
        let mut run = RunState {
            input: InputView {
                editor_id: EditorId::default(),
                buffer_id: BufferId::new(0),
                target: SubmitTarget::Run,
            },
            blocks: Vec::new(),
            scroll_offset,
            prompt: PromptInfo::default(),
            recalled: None,
        };
        let mut cmds = SlotMap::with_key();
        for n in 0..blocks {
            let mut block = Cmd::new(format!("cmd{n}"), PathBuf::from("/repo"), 10);
            block.feed(b"out");
            block.finished = n + 1 < blocks;
            run.blocks.push(cmds.insert(block));
        }
        (run, cmds)
    }

    #[test]
    fn a_wheel_walks_the_scrollback_and_stops_at_its_ends() {
        let (mut run, cmds) = scrollable(20, 0);
        let page = 5;

        run.wheel_scroll(false, page, &cmds);
        run.wheel_scroll(false, page, &cmds);
        assert_eq!(run.scroll_offset, 6, "two notches walk back six rows");

        for _ in 0..20 {
            run.wheel_scroll(false, page, &cmds);
        }
        assert_eq!(
            run.scroll_offset,
            run.output_line_total(&cmds) - page,
            "scrolling back stops with a page still shown"
        );
    }

    #[test]
    fn a_wheel_down_over_a_shrunken_scrollback_moves_the_view_at_once() {
        let (mut run, cmds) = scrollable(4, 5_000);
        let page = 5;
        let max = run.output_line_total(&cmds) - page;

        run.wheel_scroll(true, page, &cmds);

        assert_eq!(
            run.scroll_offset,
            max.saturating_sub(3),
            "an offset left over from a longer scrollback moves on the first notch"
        );
    }

    #[test]
    fn a_blocks_selection_follows_its_grid_through_a_trim() {
        let mut block = Cmd::new("cmd".to_owned(), PathBuf::from("/repo"), 10);
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

    /// The command of each block `run` holds, oldest first.
    fn commands<'a>(run: &RunState, cmds: &'a SlotMap<CmdId, Cmd>) -> Vec<&'a str> {
        run.blocks
            .iter()
            .filter_map(|id| cmds.get(*id))
            .map(|block| block.command.as_str())
            .collect()
    }

    #[test]
    fn a_flood_across_blocks_drops_the_oldest_block() {
        let (mut run, mut cmds) = scrollable(3, 0);
        for block in cmds.values_mut() {
            block.feed("x\r\n".repeat(4_000).as_bytes());
        }

        run.trim_blocks(&mut cmds);
        assert_eq!(
            commands(&run, &cmds),
            ["cmd1", "cmd2"],
            "one drop brings three 4,001-row blocks back under the cap"
        );
    }

    #[test]
    fn a_trimmed_block_leaves_the_cmd_store() {
        let (mut run, mut cmds) = scrollable(3, 0);
        for block in cmds.values_mut() {
            block.feed("x\r\n".repeat(4_000).as_bytes());
        }

        run.trim_blocks(&mut cmds);
        let stored: BTreeSet<CmdId> = cmds.keys().collect();
        let stacked: BTreeSet<CmdId> = run.blocks.iter().copied().collect();
        assert_eq!(
            (stored, run.blocks.len()),
            (stacked, 2),
            "the store holds the two ids that the stack holds"
        );
    }

    /// The older blocks each hold one row past the slack, so the pane passes
    /// the cap at any row count the flooded active block settles at.
    #[test]
    fn the_active_block_survives_any_flood() {
        let (mut run, mut cmds) = scrollable(3, 0);
        let slack = "x\r\n".repeat(vterm::SCROLLBACK_SLACK);
        cmds[run.blocks[0]].feed(slack.as_bytes());
        cmds[run.blocks[1]].feed(slack.as_bytes());
        cmds[run.blocks[2]].feed(flood().as_bytes());

        run.trim_blocks(&mut cmds);
        assert_eq!(
            commands(&run, &cmds),
            ["cmd2"],
            "both older blocks drop and the flooded active block stays"
        );
    }

    /// A trim drops the oldest finished Cmd and never one that still runs, so
    /// a flood in one Cmd kills no other process.
    #[test]
    fn a_trim_keeps_a_running_cmd() {
        let (mut run, mut cmds) = scrollable(3, 0);
        for block in cmds.values_mut() {
            block.feed("x\r\n".repeat(4_000).as_bytes());
        }
        cmds[run.blocks[0]].finished = false;

        run.trim_blocks(&mut cmds);
        assert_eq!(
            commands(&run, &cmds),
            ["cmd0", "cmd2"],
            "the running oldest Cmd stays, and the second drops"
        );
    }

    /// A trim never drops a Cmd in the Job Tray, because the tray carries it.
    #[test]
    fn a_trim_keeps_a_cmd_in_jobs() {
        let (mut run, mut cmds) = scrollable(3, 0);
        for block in cmds.values_mut() {
            block.feed("x\r\n".repeat(4_000).as_bytes());
            block.finished = true;
        }
        cmds[run.blocks[0]].in_jobs = true;

        run.trim_blocks(&mut cmds);
        assert_eq!(
            commands(&run, &cmds),
            ["cmd0", "cmd2"],
            "the oldest Cmd in Jobs stays, and the second drops"
        );
    }

    /// A block that shows its report draws no grid row, so no cell over it
    /// maps into the grid, and the raw view maps its rows again.
    #[test]
    fn a_block_showing_its_report_maps_no_cell_into_its_grid() {
        let (mut run, mut cmds) = scrollable(0, 0);
        let mut block = Cmd::new("cargo test".to_owned(), PathBuf::from("/repo"), 40);
        block.feed(ONE_FAILURE.as_bytes());
        block.finished = true;
        let id = cmds.insert(block);
        run.blocks.push(id);

        let area = Rect::new(0, 0, 44, 30);
        let mapped = |cmds: &SlotMap<CmdId, Cmd>| {
            (0..area.height)
                .flat_map(|row| (0..area.width).map(move |col| (col, row)))
                .filter(|&(col, row)| run.active_block_grid_pos(cmds, area, col, row).is_some())
                .count()
        };
        let report = mapped(&cmds);
        cmds[id].raw_view = true;
        assert_eq!((report, mapped(&cmds) > 0), (0, true));
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
