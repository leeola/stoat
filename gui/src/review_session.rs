use gpui::{App, Context, EventEmitter};
use stoat::review_session::{
    ChunkStatus, ReviewChunkId, ReviewProgress, ReviewSession as InnerSession, ReviewViewState,
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

    /// Toggle the chunk between `Staged` and `Unstaged`; chunks in
    /// `Pending` / `Skipped` flip to `Staged`. Emits
    /// [`ReviewSessionEvent::Changed`].
    pub fn toggle_stage(&mut self, id: ReviewChunkId, cx: &mut Context<'_, Self>) {
        self.inner.toggle_stage(id);
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

    /// Move the cursor to the previous chunk in order. Same
    /// semantics as [`Self::next`].
    pub fn prev(&mut self, cx: &mut Context<'_, Self>) -> Option<ReviewChunkId> {
        let id = self.inner.prev()?;
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
