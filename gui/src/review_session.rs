use gpui::{App, Context, EventEmitter};
use std::collections::BTreeSet;
use stoat::{
    review::ReviewFileInput,
    review_session::{
        ChunkStatus, ReviewChunkId, ReviewProgress, ReviewSession as InnerSession, ReviewSource,
        ReviewViewState,
    },
};

/// Entity-shaped wrapper around [`stoat::review_session::ReviewSession`].
/// Holds the underlying session and emits
/// [`ReviewSessionEvent`]s on every mutation so editors, status
/// items, and the ReviewItem render path can re-render without
/// polling.
///
/// Event fan-out:
/// - `Changed` fires on every mutation that bumps the inner version (catch-all signal for "session
///   state changed").
/// - `CursorMoved` fires in addition to `Changed` when [`Self::next`] / [`Self::prev`] actually
///   move the cursor.
/// - `Applied` and `Refreshed` are external signals emitted by the workspace's review action
///   handlers when staged chunks are applied to git or the session is rebuilt against a fresh diff
///   extraction; the wrapper provides [`Self::notify_applied`] / [`Self::notify_refreshed`] so
///   callers can fan them out without re-implementing the emit dance.
pub struct ReviewSession {
    inner: InnerSession,
    last_apply_result: Option<ReviewApplyResult>,
}

/// Outcome of a [`stoat_action::ReviewApplyStaged`] run.
///
/// `applied` is the number of staged chunks that landed in the git
/// index; `total` is the number attempted. `first_failure` carries
/// the backend's reason string for the first chunk that failed, or
/// `None` when every attempt succeeded. Consumers (status-bar
/// badges) render the success/failure shape based on whether
/// `first_failure` is `Some`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewApplyResult {
    pub applied: usize,
    pub total: usize,
    pub first_failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewSessionEvent {
    Changed,
    CursorMoved,
    Applied,
    Refreshed,
}

impl EventEmitter<ReviewSessionEvent> for ReviewSession {}

impl ReviewSession {
    pub fn new(inner: InnerSession) -> Self {
        Self {
            inner,
            last_apply_result: None,
        }
    }

    pub fn inner(&self) -> &InnerSession {
        &self.inner
    }

    pub fn last_apply_result(&self) -> Option<&ReviewApplyResult> {
        self.last_apply_result.as_ref()
    }

    /// Park the session's chunk cursor on `id` and fan out
    /// [`ReviewSessionEvent::Changed`] +
    /// [`ReviewSessionEvent::CursorMoved`] so observers re-render.
    /// Used by move-navigation action handlers that need to
    /// reposition the cursor without stepping through the order
    /// list.
    pub fn set_cursor_chunk(&mut self, id: ReviewChunkId, cx: &mut Context<'_, Self>) {
        if self.inner.cursor.current == Some(id) {
            return;
        }
        self.inner.cursor.current = Some(id);
        self.inner.version += 1;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
    }

    /// Snapshot the chunk under the review cursor into the inner
    /// session's line selection. Returns false when there is no current
    /// chunk or the chunk has no rows.
    pub fn enter_line_select(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Some(id) = self.inner.cursor.current else {
            return false;
        };
        if !self.inner.enter_line_select(id) {
            return false;
        }
        self.inner.version += 1;
        cx.emit(ReviewSessionEvent::Changed);
        cx.notify();
        true
    }

    /// Clear any active line selection.
    pub fn cancel_line_select(&mut self, cx: &mut Context<'_, Self>) {
        self.inner.cancel_line_select();
        self.inner.version += 1;
        cx.emit(ReviewSessionEvent::Changed);
        cx.notify();
    }

    /// Flip the selected bit of the active line selection's row at buffer
    /// `row`. Emits [`ReviewSessionEvent::Changed`] and returns `true`
    /// when a bit flipped; a no-op (no event) otherwise.
    pub fn toggle_line_select(&mut self, row: u32, cx: &mut Context<'_, Self>) -> bool {
        if !self.inner.toggle_line_select(row) {
            return false;
        }
        self.inner.version += 1;
        cx.emit(ReviewSessionEvent::Changed);
        cx.notify();
        true
    }

    /// Select every row of the active line selection. No-op (no event)
    /// when no selection is active.
    pub fn select_all_lines(&mut self, cx: &mut Context<'_, Self>) {
        if self.inner.select_all_lines() {
            self.inner.version += 1;
            cx.emit(ReviewSessionEvent::Changed);
            cx.notify();
        }
    }

    /// Record the result of a [`stoat_action::ReviewApplyStaged`]
    /// run and fan out [`ReviewSessionEvent::Changed`] +
    /// [`ReviewSessionEvent::Applied`] so status-bar badges and
    /// other subscribers re-render.
    pub fn set_apply_result(&mut self, result: ReviewApplyResult, cx: &mut Context<'_, Self>) {
        self.last_apply_result = Some(result);
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Applied);
        cx.notify();
    }

    /// Re-extract hunks against `new_files` and update the inner
    /// session in place. Decided chunk statuses and cursor focus
    /// carry across the refresh keyed by
    /// [`InnerSession::identity_key`]. Emits
    /// [`ReviewSessionEvent::Changed`] +
    /// [`ReviewSessionEvent::Refreshed`].
    pub fn refresh_files(&mut self, new_files: Vec<ReviewFileInput>, cx: &mut Context<'_, Self>) {
        self.inner.refresh_files(new_files);
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Refreshed);
        cx.notify();
    }

    /// Swap the inner session to `new_source` with `new_files`,
    /// preserving review decisions across the swap (see
    /// [`InnerSession::cycle_source`]). Emits
    /// [`ReviewSessionEvent::Changed`] + [`ReviewSessionEvent::Refreshed`]
    /// so the review editor rebuilds its blocks against the new file set.
    pub fn cycle_source(
        &mut self,
        new_source: ReviewSource,
        new_files: Vec<ReviewFileInput>,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.cycle_source(new_source, new_files);
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Refreshed);
        cx.notify();
    }

    /// Replace the entry for `path` with `new_input` and
    /// re-extract its hunks in isolation. No-op when `path` is
    /// not in the session's files. Emits
    /// [`ReviewSessionEvent::Changed`] +
    /// [`ReviewSessionEvent::Refreshed`].
    pub fn refresh_file(
        &mut self,
        path: &std::path::Path,
        new_input: ReviewFileInput,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.refresh_file(path, new_input);
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Refreshed);
        cx.notify();
    }

    /// Add, replace, or drop the entry for `input.path` from a fresh
    /// single-file diff (see [`InnerSession::upsert_file`]): adds the
    /// file when new, replaces its chunks when known, drops it when the
    /// diff goes empty. Returns the file's new chunk ids. Emits
    /// [`ReviewSessionEvent::Changed`] + [`ReviewSessionEvent::Refreshed`].
    pub fn upsert_file(
        &mut self,
        input: ReviewFileInput,
        cx: &mut Context<'_, Self>,
    ) -> Vec<ReviewChunkId> {
        let ids = self.inner.upsert_file(input);
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Refreshed);
        cx.notify();
        ids
    }

    pub fn progress(&self) -> ReviewProgress {
        self.inner.progress()
    }

    /// Build a fresh [`ReviewViewState`] cache from the current
    /// session state. The ReviewItem render path calls this on
    /// every `Changed` event to refresh its cached row-status
    /// data.
    pub fn view_state(&self, _cx: &App) -> ReviewViewState {
        ReviewViewState::from_session(&self.inner)
    }

    /// Mutate the chunk's status. Emits [`ReviewSessionEvent::Changed`].
    pub fn set_status(
        &mut self,
        id: ReviewChunkId,
        status: ChunkStatus,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.set_status(id, status);
        self.emit_changed(cx);
    }

    /// Commit the staged-row set and status produced by
    /// [`stoat::review_session::ReviewSession::plan_line_stage`] after its
    /// patches apply. Emits [`ReviewSessionEvent::Changed`].
    pub fn set_chunk_staged_rows(
        &mut self,
        id: ReviewChunkId,
        rows: BTreeSet<u32>,
        status: ChunkStatus,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.set_chunk_staged_rows(id, rows, status);
        self.emit_changed(cx);
    }

    /// Toggle the chunk between `Staged` and `Unstaged`; chunks in
    /// `Pending` / `Skipped` flip to `Staged`. Emits
    /// [`ReviewSessionEvent::Changed`].
    pub fn toggle_stage(&mut self, id: ReviewChunkId, cx: &mut Context<'_, Self>) {
        self.inner.toggle_stage(id);
        self.emit_changed(cx);
    }

    /// Set the chunk's approval flag. Emits
    /// [`ReviewSessionEvent::Changed`] regardless of whether the
    /// flag changed, mirroring how [`Self::set_status`] always
    /// notifies.
    pub fn set_approved(&mut self, id: ReviewChunkId, approved: bool, cx: &mut Context<'_, Self>) {
        self.inner.set_approved(id, approved);
        self.emit_changed(cx);
    }

    /// Flip the chunk's approval flag. Emits
    /// [`ReviewSessionEvent::Changed`].
    pub fn toggle_approved(&mut self, id: ReviewChunkId, cx: &mut Context<'_, Self>) {
        self.inner.toggle_approved(id);
        self.emit_changed(cx);
    }

    /// Flip follow mode on the inner session. Emits
    /// [`ReviewSessionEvent::Changed`].
    pub fn toggle_follow(&mut self, cx: &mut Context<'_, Self>) {
        self.inner.follow = !self.inner.follow;
        self.emit_changed(cx);
    }

    /// Flip live mode on the inner session. Emits
    /// [`ReviewSessionEvent::Changed`].
    pub fn toggle_live(&mut self, cx: &mut Context<'_, Self>) {
        self.inner.live = !self.inner.live;
        self.emit_changed(cx);
    }

    /// Advance the cursor to the next chunk in order. Returns the
    /// new cursor id when the cursor actually moved (in which case
    /// [`ReviewSessionEvent::Changed`] and
    /// [`ReviewSessionEvent::CursorMoved`] both fire), `None`
    /// otherwise (no event).
    pub fn next(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.next()?;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
        Some(id)
    }

    /// Move the cursor to the next unapproved chunk (wrapping past
    /// the end if necessary). Returns the new id when the cursor
    /// moved; emits `Changed` / `CursorMoved` only on a move.
    pub fn next_unreviewed(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.next_unreviewed()?;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
        Some(id)
    }

    /// Clear approval and revert status to `Pending` for every chunk,
    /// then snap the cursor back to the first chunk. Emits
    /// `Changed` and `CursorMoved`.
    pub fn reset_progress(&mut self, cx: &mut Context<'_, Self>) {
        self.inner.reset_progress();
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
    }

    /// Move the cursor to the previous chunk in order. Same
    /// semantics as [`Self::next`].
    pub fn prev(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.prev()?;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
        Some(id)
    }

    /// Move the cursor to the first chunk of the next commit group. Same
    /// event semantics as [`Self::next`].
    pub fn next_commit(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.next_commit()?;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
        Some(id)
    }

    /// Move the cursor to the first chunk of the previous commit group.
    /// Same event semantics as [`Self::next`].
    pub fn prev_commit(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.prev_commit()?;
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::CursorMoved);
        cx.notify();
        Some(id)
    }

    /// Fire [`ReviewSessionEvent::Changed`] and notify observers.
    /// Callers use this after mutating the inner session through
    /// a path the wrapper does not yet expose so subscribers
    /// refresh.
    pub fn notify_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.emit_changed(cx);
    }

    /// Signal that the session's staged chunks were applied (to
    /// git, to the working buffer, etc.). Fires both
    /// [`ReviewSessionEvent::Changed`] and
    /// [`ReviewSessionEvent::Applied`].
    pub fn notify_applied(&mut self, cx: &mut Context<'_, Self>) {
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Applied);
        cx.notify();
    }

    /// Signal that the session was rebuilt from a fresh diff
    /// extraction. Fires both [`ReviewSessionEvent::Changed`] and
    /// [`ReviewSessionEvent::Refreshed`].
    pub fn notify_refreshed(&mut self, cx: &mut Context<'_, Self>) {
        cx.emit(ReviewSessionEvent::Changed);
        cx.emit(ReviewSessionEvent::Refreshed);
        cx.notify();
    }

    fn emit_changed(&mut self, cx: &mut Context<'_, Self>) {
        cx.emit(ReviewSessionEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::review_session::ReviewSource;

    fn empty_inner() -> InnerSession {
        InnerSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::new()),
        })
    }

    fn new_session(cx: &mut TestAppContext) -> Entity<ReviewSession> {
        cx.update(|cx| cx.new(|_| ReviewSession::new(empty_inner())))
    }

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            session: &Entity<ReviewSession>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<ReviewSessionEvent>>>) {
            let events: Arc<Mutex<Vec<ReviewSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let session = session.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&session, move |_, _, event: &ReviewSessionEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn drain(events: &Arc<Mutex<Vec<ReviewSessionEvent>>>) -> Vec<ReviewSessionEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    #[test]
    fn empty_session_reports_default_progress() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        session.read_with(&cx, |s, _| {
            assert_eq!(s.progress(), ReviewProgress::default());
        });
    }

    #[test]
    fn notify_changed_emits_change_event() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| s.notify_changed(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![ReviewSessionEvent::Changed]);
    }

    #[test]
    fn notify_applied_emits_changed_and_applied() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| s.notify_applied(cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Applied],
        );
    }

    #[test]
    fn notify_refreshed_emits_changed_and_refreshed() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| s.notify_refreshed(cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Refreshed,],
        );
    }

    #[test]
    fn refresh_files_emits_changed_and_refreshed_and_updates_inner() {
        use std::path::PathBuf;
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| {
            s.refresh_files(
                vec![ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new("a\nb\n".to_string()),
                    buffer_text: Arc::new("a\nB\n".to_string()),
                }],
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Refreshed],
        );
        session.read_with(&cx, |s, _| {
            assert_eq!(s.inner().files.len(), 1);
            assert_eq!(s.inner().order.len(), 1);
        });
    }

    #[test]
    fn refresh_file_emits_changed_and_refreshed_and_updates_inner() {
        use std::path::PathBuf;
        let mut cx = TestAppContext::single();
        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                });
                inner.add_files(vec![ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new("a\nOLD\nc\n".to_string()),
                    buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
                }]);
                ReviewSession::new(inner)
            })
        });
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| {
            s.refresh_file(
                &PathBuf::from("a.txt"),
                ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new("a\nOLD\nc\n".to_string()),
                    buffer_text: Arc::new("a\nNEWER\nc\n".to_string()),
                },
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Refreshed],
        );
        session.read_with(&cx, |s, _| {
            assert_eq!(s.inner().files[0].buffer_text.as_str(), "a\nNEWER\nc\n");
        });
    }

    #[test]
    fn upsert_file_adds_new_file_and_emits_changed_and_refreshed() {
        use std::path::PathBuf;
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        let ids = session.update(&mut cx, |s, cx| {
            s.upsert_file(
                ReviewFileInput {
                    path: PathBuf::from("new.txt"),
                    rel_path: "new.txt".to_string(),
                    language: None,
                    base_text: Arc::new("a\nOLD\nc\n".to_string()),
                    buffer_text: Arc::new("a\nNEW\nc\n".to_string()),
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert!(
            !ids.is_empty(),
            "upserting a changed file returns its chunk ids"
        );
        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Refreshed],
        );
        session.read_with(&cx, |s, _| {
            assert_eq!(s.inner().files.len(), 1);
            assert_eq!(s.inner().files[0].path, PathBuf::from("new.txt"));
        });
    }

    #[test]
    fn next_on_empty_session_is_silent() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        let result = session.update(&mut cx, |s, cx| s.next(cx));
        cx.run_until_parked();

        assert_eq!(result, None);
        assert_eq!(drain(&events), Vec::<ReviewSessionEvent>::new());
    }

    #[test]
    fn prev_on_empty_session_is_silent() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        let result = session.update(&mut cx, |s, cx| s.prev(cx));
        cx.run_until_parked();

        assert_eq!(result, None);
        assert_eq!(drain(&events), Vec::<ReviewSessionEvent>::new());
    }

    #[test]
    fn set_apply_result_emits_changed_and_applied_and_stores_result() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);
        let result = ReviewApplyResult {
            applied: 2,
            total: 3,
            first_failure: Some("boom".to_string()),
        };

        session.update(&mut cx, |s, cx| s.set_apply_result(result.clone(), cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![ReviewSessionEvent::Changed, ReviewSessionEvent::Applied],
        );
        session.read_with(&cx, |s, _| {
            assert_eq!(s.last_apply_result(), Some(&result));
        });
    }

    #[test]
    fn view_state_round_trips_empty_session() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let view = session.read_with(&cx, |s, cx| s.view_state(cx));
        assert!(view.rows.is_empty());
        assert!(view.chunk_row_starts.is_empty());
        assert!(view.chunk_statuses.is_empty());
        assert!(view.current_chunk.is_none());
    }
}
