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

/// A place on screen that shows a terminal, either a split pane of a tab or a
/// dock.
///
/// The pane arm carries a tab index, because the view sits in a parked tab at
/// times. The index is good only against the workspace that gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermLocation {
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
    /// The pane's input mode.
    ///
    /// `"normal"` is the mode the pane rests in, and there its keys go to the
    /// child. Any other mode is a chord in progress, and its keys go to the
    /// keymap.
    pub mode: String,
    /// Process-unique name for this session, exported to a terminal shell as
    /// `STOAT_TERM_ID` so a command run inside it names the pane it came from.
    ///
    /// A [`TermId`] does not serve. The spawn environment is built before the
    /// PTY opens, while the id exists only after the finished session is
    /// inserted. A [`PaneId`](crate::pane::PaneId) does not serve either.
    /// Moving a pane swaps which view sits where and strands the name, while
    /// the session itself stays put.
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

    /// Pair `term` with the `session` driving it, in `"normal"` mode, where
    /// the pane sends its keys to the child.
    pub fn new(term: TermScreen, session: Arc<dyn TerminalSession>, token: u64) -> Self {
        Self {
            term,
            session,
            selection: None,
            mode: "normal".into(),
            token,
        }
    }

    /// The selected text, or `None` when nothing is selected or the selection
    /// covers only blank cells.
    ///
    /// The selection reads as [`TermScreen::span_text`] reads a span, which is
    /// how a terminal copies a selection.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection?.ordered();
        let text = self.term.span_text(start, end);
        (!text.trim().is_empty()).then_some(text)
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

    /// A copy reads each character once with its marks, and a tab as the tab
    /// the program wrote, up to the next tab stop.
    #[test]
    fn selection_text_reads_characters_as_written() {
        assert_eq!(
            [
                read_span("a\u{4e2d}b", (0, 0), (0, 3)),
                read_span("\u{4e2d}b", (0, 1), (0, 2)),
                read_span("e\u{301}x", (0, 0), (0, 1)),
                read_span("a\t  b", (0, 0), (0, 10)),
            ],
            ["a\u{4e2d}b", "\u{4e2d}b", "e\u{301}x", "a\t  b"].map(|text| Some(text.to_string())),
            "a wide character, one from its second column, a combining mark, a tab",
        );
    }

    /// A row the program wrapped joins the next one with no line break. A span
    /// that ends on the spacer a wrapped wide character leaves at the end of the
    /// top row reads up to that spacer.
    #[test]
    fn selection_text_joins_a_wrapped_row() {
        let wide = format!("{}\u{4e2d}", "x".repeat(19));
        assert_eq!(
            [
                read_span(&"x".repeat(25), (0, 0), (1, 4)),
                read_span(&wide, (0, 0), (1, 1)),
                read_span(&wide, (0, 0), (0, 19)),
            ],
            [
                Some("x".repeat(25)),
                Some(wide.clone()),
                Some("x".repeat(19))
            ],
        );
    }

    /// The text a selection from `anchor` to `head` copies on a screen fed
    /// `text`.
    fn read_span(text: &str, anchor: (usize, usize), head: (usize, usize)) -> Option<String> {
        let mut session = session_with(text.as_bytes());
        session.selection = Some(selection(anchor, head));
        session.selection_text()
    }
}
