use gpui::{Context, EventEmitter};
use stoat::{
    host::LspNotification,
    lsp::progress::{LspProgressEntry, LspProgressMap},
};

/// Entity-shaped wrapper around stoat's LSP progress state. LSP-side
/// wiring pushes incoming notifications through
/// [`LspState::update`]; the wrapper emits
/// [`LspStateEvent::Changed`] when a Progress variant moves the
/// state forward so the status bar re-paints. Today the inner type
/// tracks a single `LspServer`; a future `LspManager` widens the key
/// to `(LanguageServerId, ProgressToken)` and additional
/// per-server status fields slot in without changing the event
/// surface.
pub struct LspState {
    inner: LspProgressMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspStateEvent {
    Changed,
}

impl EventEmitter<LspStateEvent> for LspState {}

impl LspState {
    pub fn new() -> Self {
        Self {
            inner: LspProgressMap::new(),
        }
    }

    /// Push a notification through the underlying progress map.
    /// Returns `true` and emits Changed when the call advanced
    /// state (a Progress variant was recognised); `false` for
    /// non-Progress notifications, which the inner type ignores.
    pub fn update(&mut self, notification: &LspNotification, cx: &mut Context<'_, Self>) -> bool {
        let changed = self.inner.update(notification);
        if changed {
            cx.emit(LspStateEvent::Changed);
            cx.notify();
        }
        changed
    }

    pub fn current(&self) -> Option<&LspProgressEntry> {
        self.inner.current()
    }

    pub fn is_idle(&self) -> bool {
        self.inner.current().is_none()
    }
}

impl Default for LspState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use lsp_types::{
        MessageType, NumberOrString, ProgressToken, WorkDoneProgress, WorkDoneProgressBegin,
        WorkDoneProgressEnd, WorkDoneProgressReport,
    };
    use std::sync::{Arc, Mutex};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            state: &Entity<LspState>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<LspStateEvent>>>) {
            let events: Arc<Mutex<Vec<LspStateEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let state = state.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&state, move |_, _, event: &LspStateEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<LspStateEvent>>>) -> Vec<LspStateEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn token(id: i32) -> ProgressToken {
        NumberOrString::Number(id)
    }

    fn begin(title: &str, percentage: Option<u32>) -> LspNotification {
        LspNotification::Progress {
            token: token(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: title.to_owned(),
                cancellable: None,
                message: None,
                percentage,
            }),
        }
    }

    fn report(message: Option<&str>, percentage: Option<u32>) -> LspNotification {
        LspNotification::Progress {
            token: token(1),
            value: WorkDoneProgress::Report(WorkDoneProgressReport {
                cancellable: None,
                message: message.map(str::to_owned),
                percentage,
            }),
        }
    }

    fn end() -> LspNotification {
        LspNotification::Progress {
            token: token(1),
            value: WorkDoneProgress::End(WorkDoneProgressEnd { message: None }),
        }
    }

    fn new_state(cx: &mut TestAppContext) -> Entity<LspState> {
        cx.update(|cx| cx.new(|_| LspState::new()))
    }

    #[test]
    fn new_state_reports_idle_and_no_current() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);

        assert!(state.read_with(&cx, |s, _| s.is_idle()));
        assert!(state.read_with(&cx, |s, _| s.current().is_none()));
    }

    #[test]
    fn begin_push_emits_changed_and_sets_current() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        let changed = state.update(&mut cx, |s, cx| s.update(&begin("indexing", Some(10)), cx));
        cx.run_until_parked();

        assert!(changed);
        assert_eq!(drain(&events), vec![LspStateEvent::Changed]);
        let (title, percentage) = state
            .read_with(&cx, |s, _| {
                s.current().map(|e| (e.title.clone(), e.percentage))
            })
            .expect("current entry present");
        assert_eq!(title, "indexing");
        assert_eq!(percentage, Some(10));
        assert!(!state.read_with(&cx, |s, _| s.is_idle()));
    }

    #[test]
    fn report_after_begin_emits_and_updates_entry() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        state.update(&mut cx, |s, cx| s.update(&begin("indexing", Some(10)), cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        state.update(&mut cx, |s, cx| {
            s.update(&report(Some("phase 2"), Some(50)), cx)
        });
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![LspStateEvent::Changed]);
        let (message, percentage) = state
            .read_with(&cx, |s, _| {
                s.current().map(|e| (e.message.clone(), e.percentage))
            })
            .expect("current entry present");
        assert_eq!(message.as_deref(), Some("phase 2"));
        assert_eq!(percentage, Some(50));
    }

    #[test]
    fn end_push_emits_changed_and_returns_to_idle() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        state.update(&mut cx, |s, cx| s.update(&begin("indexing", None), cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        state.update(&mut cx, |s, cx| s.update(&end(), cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![LspStateEvent::Changed]);
        assert!(state.read_with(&cx, |s, _| s.is_idle()));
    }

    #[test]
    fn non_progress_notification_is_a_no_op() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        let log = LspNotification::LogMessage {
            typ: MessageType::INFO,
            message: "hello".into(),
        };
        let changed = state.update(&mut cx, |s, cx| s.update(&log, cx));
        cx.run_until_parked();

        assert!(!changed);
        assert_eq!(drain(&events), Vec::<LspStateEvent>::new());
        assert!(state.read_with(&cx, |s, _| s.is_idle()));
    }
}
