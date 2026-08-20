//! Per-session PTY state for a pane running a terminal shell or an agent.
//!
//! Bundles the [`TermScreen`] screen emulator with the [`TerminalSession`]
//! whose PTY output feeds it. The workspace owns a collection of these so it
//! can host several sessions at once, and a pane view such as
//! [`View::Agent`](crate::pane::View::Agent) names one by its [`TermId`].

use crate::{
    host::terminal::TerminalSession,
    pane::{DockId, PaneId},
    term_screen::TermScreen,
};
use futures::FutureExt;
use slotmap::new_key_type;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Source of [`TermSession::token`] values, handed out by
/// [`TermSession::next_token`].
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

new_key_type! {
    /// Workspace-scoped key for a [`TermSession`] in the workspace's term
    /// collection.
    pub struct TermId;
}

/// A linear text selection over a terminal viewport's cells.
///
/// `anchor` is the cell where the drag began and `head` the cell it currently
/// reaches, both `(row, col)` and viewport-relative. The selection runs in
/// reading order between the two regardless of drag direction, so a row lying
/// fully between the endpoints is selected end to end.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TermSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl TermSelection {
    /// A zero-width selection anchored at `(row, col)`, before a drag extends it.
    pub fn new(row: usize, col: usize) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
        }
    }

    /// Move the reaching end to `(row, col)`, leaving the anchor fixed, and
    /// report whether that moved it.
    ///
    /// A terminal in any-motion tracking reports a drag per pointer motion
    /// rather than per cell, so a sweep across one character arrives many
    /// times over. The answer is what lets a caller drop the repeats.
    pub fn extend_to(&mut self, row: usize, col: usize) -> bool {
        let moved = self.head != (row, col);
        self.head = (row, col);
        moved
    }

    /// The endpoints in reading order, `(start, end)` with `start <= end`.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether the cell `(row, col)` falls within the selection, inclusive of
    /// both endpoints.
    pub(crate) fn contains(&self, row: usize, col: usize) -> bool {
        let ((start_row, start_col), (end_row, end_col)) = self.ordered();
        if row < start_row || row > end_row {
            return false;
        }
        let after_start = row > start_row || col >= start_col;
        let before_end = row < end_row || col <= end_col;
        after_start && before_end
    }
}

/// Where focus sat when it last arrived on a terminal, so `Esc` can send it
/// back there.
///
/// A terminal pane has no editing state of its own, which makes its normal mode
/// a dead end. Remembering the origin turns `Esc` into the inverse of whatever
/// motion reached the terminal.
///
/// The pane arm carries a tab index because a return can cross tabs, and the
/// index is only meaningful against the workspace that recorded it. Both arms
/// are validated at use, since a pane or dock can be closed while the record
/// still names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermReturnFocus {
    Pane { tab: usize, pane: PaneId },
    Dock(DockId),
}

/// A live term session pairing its screen emulator with the PTY session that
/// drives it.
///
/// The [`TerminalSession`] is held as an [`Arc`] so a background reader can
/// pull PTY output into [`Self::term`] while the app loop still writes input
/// to the same session.
pub struct TermSession {
    pub term: TermScreen,
    pub session: Arc<dyn TerminalSession>,
    /// The active mouse selection over the screen, or `None` when nothing is
    /// selected. Set while dragging, kept highlighted after release for the copy,
    /// and cleared by the next keystroke, click, or new drag.
    pub selection: Option<TermSelection>,
    /// The pane's input mode. `"insert"` enables PTY passthrough so keys reach
    /// the child, while other modes keep stoat's pane-level bindings live.
    ///
    /// Held per-term, but a [`View::Terminal`](crate::pane::View::Terminal) pane
    /// is forced to insert whenever focus arrives on it, so only a
    /// [`View::Agent`](crate::pane::View::Agent) pane preserves a non-insert
    /// mode across focus changes.
    pub mode: String,
    /// Where focus came from when it last arrived on this terminal, or `None`
    /// when it was never reached by a focus motion.
    ///
    /// Overwritten on every arrival, so terminal-to-terminal hops ping-pong.
    /// The record is deliberately not persisted. Sessions die with the process,
    /// and a respawned shell starts with no history to return to.
    pub(crate) return_focus: Option<TermReturnFocus>,
    /// Process-unique name for this session, exported to a terminal shell as
    /// `STOAT_TERM_ID` so a command run inside it names the pane it came from.
    ///
    /// A [`TermId`] does not serve. The spawn environment is built before the
    /// PTY opens, while the id exists only after the finished session is
    /// inserted. A [`PaneId`] does not serve either. Moving a pane swaps which
    /// view sits where and strands the name, while the session itself stays
    /// put.
    pub token: u64,
}

impl TermSession {
    /// A token no other session in this process holds.
    ///
    /// Minted before the spawn so the value is available to put in the child's
    /// environment, then handed to [`Self::new`] to record on the session it
    /// names.
    pub(crate) fn next_token() -> u64 {
        NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
    }

    /// Pair `term` with the `session` driving it, opening in `"normal"` mode.
    ///
    /// A [`View::Terminal`](crate::pane::View::Terminal) pane is flipped to
    /// insert when focus arrives, so this initial normal mode is what a
    /// [`View::Agent`](crate::pane::View::Agent) pane holds until the user
    /// presses `i`.
    pub fn new(term: TermScreen, session: Arc<dyn TerminalSession>, token: u64) -> Self {
        Self {
            term,
            session,
            selection: None,
            mode: "normal".into(),
            return_focus: None,
            token,
        }
    }

    /// The selected text, or `None` when nothing is selected or the selection
    /// covers only blank cells.
    ///
    /// Each row's selected span is read from the screen with its trailing blanks
    /// trimmed, then the rows are joined with newlines, matching how a terminal
    /// copies a multi-line selection.
    pub fn selection_text(&self) -> Option<String> {
        let ((start_row, start_col), (end_row, end_col)) = self.selection?.ordered();
        let cols = self.term.cols();

        let mut out = String::new();
        for row in start_row..=end_row {
            let cells = self.term.row(row);
            let from = if row == start_row { start_col } else { 0 };
            let to = if row == end_row {
                (end_col + 1).min(cols)
            } else {
                cols
            };
            let span = cells.get(from..to.min(cells.len())).unwrap_or(&[]);
            let line: String = span.iter().map(|cell| cell.ch).collect();

            if row != start_row {
                out.push('\n');
            }
            out.push_str(line.trim_end());
        }

        (!out.trim().is_empty()).then_some(out)
    }

    /// Resize the emulator and its PTY to `rows` by `cols` so the child reflows
    /// to the hosting pane.
    ///
    /// A no-op when the emulator already matches, which keeps per-frame layout
    /// from issuing a redundant PTY resize (and the SIGWINCH redraw storm it
    /// would trigger in the child) on every frame.
    pub fn fit(&mut self, rows: u16, cols: u16) {
        if self.term.rows() == rows as usize && self.term.cols() == cols as usize {
            return;
        }

        let replies = self.term.resize(rows, cols);
        if let Err(err) = self.session.resize(rows, cols) {
            tracing::warn!(target: "stoat::agent", %err, "failed to resize agent pty");
        }
        if !replies.is_empty() {
            let _ = self.session.write(&replies).now_or_never();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TermSelection, TermSession};
    use crate::{host::FakeTerminalSession, term_screen::TermScreen};
    use std::{collections::HashSet, sync::Arc};

    fn session_with(text: &[u8]) -> TermSession {
        let mut term = TermScreen::new(4, 20);
        term.feed(text);
        TermSession::new(
            term,
            Arc::new(FakeTerminalSession::new()),
            TermSession::next_token(),
        )
    }

    fn selection(anchor: (usize, usize), head: (usize, usize)) -> TermSelection {
        let mut sel = TermSelection::new(anchor.0, anchor.1);
        sel.extend_to(head.0, head.1);
        sel
    }

    // Other tests mint tokens on their own threads, so this asserts only
    // distinctness, never the values or the step between them.
    #[test]
    fn tokens_are_never_reissued() {
        let minted: HashSet<u64> = (0..4).map(|_| TermSession::next_token()).collect();
        assert_eq!(minted.len(), 4, "each session gets a name of its own");
    }

    #[test]
    fn contains_spans_full_middle_rows_in_reading_order() {
        let sel = selection((0, 3), (2, 1));
        assert!(
            !sel.contains(0, 2),
            "a col before the anchor on the first row is out"
        );
        assert!(sel.contains(0, 3), "the anchor cell is in");
        assert!(
            sel.contains(1, 19),
            "any col on a fully-spanned middle row is in"
        );
        assert!(sel.contains(2, 1), "the head cell is in");
        assert!(
            !sel.contains(2, 2),
            "a col past the head on the last row is out"
        );
    }

    #[test]
    fn selection_text_reads_a_single_row_span() {
        let mut session = session_with(b"hello world");
        session.selection = Some(selection((0, 0), (0, 4)));
        assert_eq!(session.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selection_text_joins_rows_and_trims_trailing_blanks() {
        let mut session = session_with(b"abc\r\ndef");
        session.selection = Some(selection((0, 0), (1, 2)));
        assert_eq!(session.selection_text().as_deref(), Some("abc\ndef"));
    }

    #[test]
    fn selection_text_is_none_over_blank_cells() {
        let mut session = session_with(b"");
        session.selection = Some(selection((0, 0), (0, 5)));
        assert_eq!(session.selection_text(), None);
    }
}
