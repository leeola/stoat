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
use alacritty_terminal::selection::SelectionRange;
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

/// Where a view shows, which is a split or detached pane of a tab, or a dock.
///
/// The pane arm carries a tab index, because the view sits in a parked tab at
/// times. The index is good only against the workspace that gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewLocation {
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
            mode: "normal".into(),
            token,
        }
    }

    /// The mouse selection the pane paints and copies, in grid points, or
    /// `None` when no selection marks a cell.
    ///
    /// A press marks no cell before a drag extends it, and the release that
    /// copies a drag clears it, as do the next keystroke, click, or wheel
    /// notch. See [`TermScreen::selection_range`].
    pub(crate) fn painted_selection(&self) -> Option<SelectionRange> {
        self.term.selection_range()
    }

    /// The selected text, or `None` when no selection marks a cell or the
    /// selection covers only blank cells. See [`TermScreen::selection_text`].
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_text()
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
    use super::TermSession;
    use crate::test_fixture::fake_term_session;
    use std::collections::HashSet;

    fn session_with(text: &[u8]) -> TermSession {
        let (mut session, _) = fake_term_session(4, 20);
        session.term.feed(text);
        session
    }

    /// Select from viewport cell `anchor` to `head`, both `(row, col)`, as a
    /// press and a drag do.
    fn select(session: &mut TermSession, anchor: (usize, usize), head: (usize, usize)) {
        session.term.start_selection(anchor.0, anchor.1);
        session.term.extend_selection(head.0, head.1);
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
        let mut session = session_with(b"");
        select(&mut session, (0, 3), (2, 1));
        let range = session.painted_selection().expect("the drag marked cells");
        let contains = |row, col| range.contains(session.term.grid_point(row, col));
        assert!(
            !contains(0, 2),
            "a col before the anchor on the first row is out"
        );
        assert!(contains(0, 3), "the anchor cell is in");
        assert!(
            contains(1, 19),
            "any col on a fully-spanned middle row is in"
        );
        assert!(contains(2, 1), "the head cell is in");
        assert!(
            !contains(2, 2),
            "a col past the head on the last row is out"
        );
    }

    #[test]
    fn selection_text_reads_a_single_row_span() {
        let mut session = session_with(b"hello world");
        select(&mut session, (0, 0), (0, 4));
        assert_eq!(session.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selection_text_joins_rows_and_trims_trailing_blanks() {
        let mut session = session_with(b"abc\r\ndef");
        select(&mut session, (0, 0), (1, 2));
        assert_eq!(session.selection_text().as_deref(), Some("abc\ndef"));
    }

    #[test]
    fn selection_text_is_none_over_blank_cells() {
        let mut session = session_with(b"");
        select(&mut session, (0, 0), (0, 5));
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
        select(&mut session, anchor, head);
        session.selection_text()
    }
}
