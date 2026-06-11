use gpui::{Context, EventEmitter};
use lsp_types::Diagnostic;
use std::path::{Path, PathBuf};
use stoat::diagnostics::{DiagnosticSet as InnerSet, DiagnosticSummary};

/// Entity-shaped wrapper around [`stoat::diagnostics::DiagnosticSet`].
/// LSP-side writers push the latest diagnostic list per path via
/// [`DiagnosticSet::replace_for_path`]; the wrapper emits
/// [`DiagnosticSetEvent::Changed`] so editor gutters and the status
/// bar re-render.
pub struct DiagnosticSet {
    inner: InnerSet,
    /// Bumped on every mutation so readers (e.g. the scrollbar-marker
    /// cache) can detect a change without diffing the diagnostic lists.
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticSetEvent {
    Changed { path: PathBuf },
}

impl EventEmitter<DiagnosticSetEvent> for DiagnosticSet {}

impl DiagnosticSet {
    pub fn new() -> Self {
        Self {
            inner: InnerSet::new(),
            generation: 0,
        }
    }

    /// Monotonic counter bumped on every mutation. A cache keyed on it
    /// recomputes only when the diagnostics actually changed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn replace_for_path(
        &mut self,
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
        cx: &mut Context<'_, Self>,
    ) {
        self.inner.replace_for_path(path.clone(), diagnostics);
        self.generation += 1;
        cx.emit(DiagnosticSetEvent::Changed { path });
        cx.notify();
    }

    pub fn clear(&mut self, path: PathBuf, cx: &mut Context<'_, Self>) {
        self.replace_for_path(path, Vec::new(), cx);
    }

    pub fn get(&self, path: &Path) -> &[Diagnostic] {
        self.inner.get(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Path, &[Diagnostic])> {
        self.inner.iter()
    }

    pub fn summarize(&self, path: &Path) -> DiagnosticSummary {
        self.inner.summarize(path)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.iter().next().is_none()
    }
}

impl Default for DiagnosticSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Subscription, TestAppContext};
    use lsp_types::{DiagnosticSeverity, Position, Range};
    use std::sync::{Arc, Mutex};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            set: &Entity<DiagnosticSet>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<DiagnosticSetEvent>>>) {
            let events: Arc<Mutex<Vec<DiagnosticSetEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let set = set.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&set, move |_, _, event: &DiagnosticSetEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<DiagnosticSetEvent>>>) -> Vec<DiagnosticSetEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn diag(severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn new_set(cx: &mut TestAppContext) -> Entity<DiagnosticSet> {
        cx.update(|cx| cx.new(|_| DiagnosticSet::new()))
    }

    #[test]
    fn replace_for_path_stores_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let set = new_set(&mut cx);
        let (_recorder, events) = Recorder::install(&mut cx, &set);
        let path = PathBuf::from("/ws/a.rs");

        set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "e")], cx)
        });
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![DiagnosticSetEvent::Changed { path: path.clone() }]
        );
        let count = set.read_with(&cx, |s, _| s.get(&path).len());
        assert_eq!(count, 1);
    }

    #[test]
    fn replace_with_empty_clears_entry_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let set = new_set(&mut cx);
        let path = PathBuf::from("/ws/a.rs");
        set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "e")], cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &set);

        set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), Vec::new(), cx)
        });
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![DiagnosticSetEvent::Changed { path: path.clone() }]
        );
        assert!(set.read_with(&cx, |s, _| s.get(&path).is_empty()));
        assert!(set.read_with(&cx, |s, _| s.is_empty()));
    }

    #[test]
    fn clear_helper_mirrors_empty_replace() {
        let mut cx = TestAppContext::single();
        let set = new_set(&mut cx);
        let path = PathBuf::from("/ws/a.rs");
        set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "e")], cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &set);

        set.update(&mut cx, |s, cx| s.clear(path.clone(), cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![DiagnosticSetEvent::Changed { path: path.clone() }]
        );
        assert!(set.read_with(&cx, |s, _| s.get(&path).is_empty()));
    }

    #[test]
    fn summarize_after_replace_reflects_counts() {
        let mut cx = TestAppContext::single();
        let set = new_set(&mut cx);
        let path = PathBuf::from("/ws/a.rs");

        set.update(&mut cx, |s, cx| {
            s.replace_for_path(
                path.clone(),
                vec![
                    diag(DiagnosticSeverity::ERROR, "e1"),
                    diag(DiagnosticSeverity::WARNING, "w1"),
                    diag(DiagnosticSeverity::WARNING, "w2"),
                ],
                cx,
            )
        });

        let summary = set.read_with(&cx, |s, _| s.summarize(&path));
        assert_eq!(summary.error, 1);
        assert_eq!(summary.warning, 2);
        assert_eq!(summary.worst, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn multiple_paths_are_independent() {
        let mut cx = TestAppContext::single();
        let set = new_set(&mut cx);
        let a = PathBuf::from("/ws/a.rs");
        let b = PathBuf::from("/ws/b.rs");

        set.update(&mut cx, |s, cx| {
            s.replace_for_path(a.clone(), vec![diag(DiagnosticSeverity::ERROR, "ea")], cx);
            s.replace_for_path(b.clone(), vec![diag(DiagnosticSeverity::WARNING, "wb")], cx);
        });

        assert_eq!(set.read_with(&cx, |s, _| s.get(&a).len()), 1);
        assert_eq!(set.read_with(&cx, |s, _| s.get(&b).len()), 1);
        assert_eq!(
            set.read_with(&cx, |s, _| s.summarize(&a).worst),
            Some(DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            set.read_with(&cx, |s, _| s.summarize(&b).worst),
            Some(DiagnosticSeverity::WARNING)
        );
    }
}
