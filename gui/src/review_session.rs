use gpui::{Context, EventEmitter};
use stoat::review_session::{ReviewProgress, ReviewSession as InnerSession};

/// Entity-shaped wrapper around [`stoat::review_session::ReviewSession`].
/// Holds the underlying session and emits
/// [`ReviewSessionEvent::Changed`] whenever the inner state mutates,
/// so editors and status items can re-render without polling.
///
/// The mutation surface is intentionally minimal in this slice; the
/// review pipeline that opens sessions and routes navigation /
/// staging actions lands in sibling items in
/// `.todo-plans/TODO.md`. Until then the wrapper exposes read access
/// to the inner session plus an explicit [`Self::notify_changed`]
/// signal that callers fire after they have mutated the inner
/// session through some other path.
pub struct ReviewSession {
    inner: InnerSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewSessionEvent {
    Changed,
}

impl EventEmitter<ReviewSessionEvent> for ReviewSession {}

impl ReviewSession {
    pub fn new(inner: InnerSession) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &InnerSession {
        &self.inner
    }

    pub fn progress(&self) -> ReviewProgress {
        self.inner.progress()
    }

    /// Fire [`ReviewSessionEvent::Changed`] and notify observers.
    /// Callers use this after mutating the inner session through a
    /// path the wrapper does not yet expose (for example via the
    /// owning workspace) so subscribers refresh.
    pub fn notify_changed(&mut self, cx: &mut Context<'_, Self>) {
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

    #[test]
    fn empty_session_reports_default_progress() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        session.read_with(&cx, |s, _| {
            assert_eq!(s.progress(), ReviewProgress::default());
        });
    }

    #[test]
    fn notify_changed_emits_event() {
        let mut cx = TestAppContext::single();
        let session = new_session(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &session);

        session.update(&mut cx, |s, cx| s.notify_changed(cx));
        cx.run_until_parked();

        assert_eq!(
            events.lock().expect("recorder mutex").clone(),
            vec![ReviewSessionEvent::Changed],
        );
    }
}
