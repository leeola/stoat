mod delegate;

use crate::{
    editor::{Editor, EditorEvent},
    modal_layer::ModalView,
};
pub use delegate::{PickerDelegate, PickerSecondary};
use gpui::{
    div, uniform_list, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Task, UniformListScrollHandle, Window,
};
pub use stoat::fuzzy::{match_and_rank, RankedMatch};

/// Filter and rank `(item, haystack)` pairs against `query` using
/// the shared nucleo matcher and the picker-side score-descending
/// convention.
///
/// Returns `None` when `query` produces no usable atoms -- the
/// caller falls back to its natural ordering (alphabetical,
/// priority, etc.) in that case. The returned `Vec` is sorted by
/// [`RankedMatch::score`] descending; ties keep their input order
/// so callers can apply a secondary tie-break (basename,
/// registration order, ...) with a stable sort afterward.
pub fn rank_matches<T>(
    query: &str,
    items: impl IntoIterator<Item = (T, String)>,
) -> Option<Vec<RankedMatch<T>>> {
    let mut matches = match_and_rank(query, items)?;
    matches.sort_by_key(|m| std::cmp::Reverse(m.score));
    Some(matches)
}

/// Stoat-native picker container. Owns the query editor, the result
/// list's scroll handle, the focus handle pushed into gpui by the
/// modal layer, and the in-flight `update_matches` task; delegates
/// to `D` for items, the active cursor, and per-row rendering.
///
/// The container subscribes to the query editor's
/// [`EditorEvent::Changed`] signal. On every change it reads the
/// query text out of the editor's singleton buffer and hands it to
/// [`PickerDelegate::update_matches`]; the returned [`Task`] is
/// stored on the picker so a fresh edit cancels the prior walk by
/// dropping its task handle.
///
/// Action dispatch flows in through [`Picker::handle_action`]; the
/// modifier-routed confirmations follow-up adds the per-`ActionKind`
/// arms (select next/prev, confirm primary/secondary, dismiss). At
/// this stage the method is wired but its match table is empty so
/// the picker is keyboard-inert except for the query editor's own
/// IME-routed insert path.
pub struct Picker<D: PickerDelegate> {
    delegate: D,
    query_editor: Entity<Editor>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    pending_update_matches: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl<D: PickerDelegate> Picker<D> {
    pub fn new(delegate: D, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let query_editor = cx.new(|cx| Editor::single_line(window, cx));
        let subscription =
            cx.subscribe(
                &query_editor,
                |this, _editor, event: &EditorEvent, cx| match event {
                    EditorEvent::Changed => {
                        let query = current_query(&this.query_editor, cx);
                        let task = this.delegate.update_matches(query, cx);
                        this.pending_update_matches = Some(task);
                    },
                },
            );
        Self {
            delegate,
            query_editor,
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            pending_update_matches: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn delegate(&self) -> &D {
        &self.delegate
    }

    pub fn delegate_mut(&mut self) -> &mut D {
        &mut self.delegate
    }

    pub fn query_editor(&self) -> &Entity<Editor> {
        &self.query_editor
    }

    pub fn selected_index(&self) -> usize {
        self.delegate.selected_index()
    }

    pub fn set_selected_index(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.delegate.set_selected_index(ix, cx);
        cx.notify();
    }

    /// Route an action resolved by the workspace's input pipeline
    /// into the picker. The match table for select-next /
    /// select-prev / confirm / dismiss lands with the
    /// modifier-routed confirmations follow-up; this method holds
    /// the dispatch surface in place so workspace wiring can target
    /// the active picker without waiting on the action wiring.
    pub fn handle_action(
        &mut self,
        action: Box<dyn stoat_action::Action>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) {
        let _ = action;
    }
}

fn current_query<D: PickerDelegate>(
    query_editor: &Entity<Editor>,
    cx: &Context<'_, Picker<D>>,
) -> String {
    let editor = query_editor.read(cx);
    let multi_buffer = editor.multi_buffer().read(cx);
    multi_buffer
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

impl<D: PickerDelegate> Render for Picker<D> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let handle = cx.entity().downgrade();
        let count = self.delegate.match_count();
        let list = uniform_list("picker-rows", count, move |range, _window, cx| {
            let Some(picker) = handle.upgrade() else {
                return Vec::new();
            };
            picker.update(cx, |this, cx| {
                let selected = this.delegate.selected_index();
                range
                    .map(|ix| this.delegate.render_match(ix, ix == selected, cx))
                    .collect()
            })
        })
        .track_scroll(self.scroll_handle.clone())
        .flex_grow();

        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.query_editor.clone())
            .child(list)
    }
}

impl<D: PickerDelegate> Focusable for Picker<D> {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<D: PickerDelegate> EventEmitter<DismissEvent> for Picker<D> {}

impl<D: PickerDelegate> ModalView for Picker<D> {
    fn key_context_name(&self, _cx: &gpui::App) -> Option<SharedString> {
        Some("Picker".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AnyElement, AppContext, TestAppContext, VisualTestContext};
    use std::sync::{Arc, Mutex};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    struct TestDelegate {
        items: Vec<String>,
        selected: usize,
        history: Arc<Mutex<Vec<String>>>,
        confirmed: Arc<Mutex<Vec<Option<PickerSecondary>>>>,
        dismissed: Arc<Mutex<u32>>,
    }

    impl TestDelegate {
        fn new(items: Vec<String>) -> Self {
            Self {
                items,
                selected: 0,
                history: Arc::new(Mutex::new(Vec::new())),
                confirmed: Arc::new(Mutex::new(Vec::new())),
                dismissed: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl PickerDelegate for TestDelegate {
        fn match_count(&self) -> usize {
            self.items.len()
        }

        fn selected_index(&self) -> usize {
            self.selected
        }

        fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
            self.selected = ix;
        }

        fn update_matches(
            &mut self,
            query: String,
            _cx: &mut Context<'_, Picker<Self>>,
        ) -> Task<()> {
            self.history.lock().expect("test history mutex").push(query);
            Task::ready(())
        }

        fn confirm(
            &mut self,
            secondary: Option<PickerSecondary>,
            _cx: &mut Context<'_, Picker<Self>>,
        ) {
            self.confirmed
                .lock()
                .expect("test confirmed mutex")
                .push(secondary);
        }

        fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {
            *self.dismissed.lock().expect("test dismissed mutex") += 1;
        }

        fn render_match(
            &self,
            ix: usize,
            _selected: bool,
            _cx: &mut Context<'_, Picker<Self>>,
        ) -> AnyElement {
            div().child(self.items[ix].clone()).into_any_element()
        }
    }

    struct Harness<'a> {
        picker: Entity<Picker<TestDelegate>>,
        history: Arc<Mutex<Vec<String>>>,
        vcx: &'a mut VisualTestContext,
    }

    fn new_picker(cx: &mut TestAppContext, items: Vec<String>) -> Harness<'_> {
        install_executor_global(cx);
        let delegate = TestDelegate::new(items);
        let history = delegate.history.clone();
        let vcx = cx.add_empty_window();
        let picker = vcx.update(|window, cx| cx.new(|cx| Picker::new(delegate, window, cx)));
        Harness {
            picker,
            history,
            vcx,
        }
    }

    #[test]
    fn new_picker_starts_with_empty_query_and_first_selection() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into(), "gamma".into()]);
        h.vcx.run_until_parked();

        let (selected, count) = h.picker.read_with(h.vcx, |p, _| {
            (p.selected_index(), p.delegate().match_count())
        });
        assert_eq!((selected, count), (0, 3));
    }

    #[test]
    fn editing_query_calls_update_matches_with_buffer_text() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into()]);
        let query_editor = h.picker.read_with(h.vcx, |p, _| p.query_editor().clone());

        let buffer = query_editor.read_with(h.vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("single-line editor has singleton buffer")
                .clone()
        });
        buffer.update(h.vcx, |b, cx| b.edit(0..0, "foo", cx));
        h.vcx.run_until_parked();

        let recorded = h.history.lock().expect("test history mutex").clone();
        assert!(
            !recorded.is_empty() && recorded.iter().all(|q| q == "foo"),
            "expected every recorded query to be \"foo\", got {recorded:?}",
        );
    }

    #[test]
    fn set_selected_index_forwards_to_delegate() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into()]);
        h.picker.update(h.vcx, |p, cx| p.set_selected_index(1, cx));

        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 1);
    }

    #[test]
    fn handle_action_is_a_noop_until_action_wiring_lands() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(
                    Box::new(crate::actions::SetActivePane { pane_id: 0 }),
                    window,
                    cx,
                )
            });
        });
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 0);
    }

    fn items(pairs: &[(usize, &str)]) -> Vec<(usize, String)> {
        pairs.iter().map(|(i, s)| (*i, (*s).to_string())).collect()
    }

    #[test]
    fn rank_matches_returns_none_for_empty_query() {
        assert!(rank_matches("", items(&[(0, "alpha"), (1, "beta")])).is_none());
        assert!(rank_matches("   ", items(&[(0, "alpha"), (1, "beta")])).is_none());
    }

    #[test]
    fn rank_matches_sorts_by_score_descending() {
        let ranked = rank_matches(
            "foo",
            items(&[(0, "barfoo"), (1, "foo.rs"), (2, "f_o_o"), (3, "foobar")]),
        )
        .expect("query has atoms");
        let order: Vec<usize> = ranked.iter().map(|m| m.item).collect();
        let scores: Vec<u32> = ranked.iter().map(|m| m.score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "scores not descending: {scores:?} (order {order:?})",
        );
    }

    #[test]
    fn rank_matches_filters_non_matches() {
        let ranked = rank_matches("foo", items(&[(0, "alpha"), (1, "bravo"), (2, "foobar")]))
            .expect("query has atoms");
        let kept: Vec<usize> = ranked.iter().map(|m| m.item).collect();
        assert_eq!(kept, vec![2]);
    }

    #[test]
    fn rank_matches_propagates_matched_indices() {
        let ranked = rank_matches("foo", items(&[(0, "foo.rs")])).expect("query has atoms");
        assert_eq!(ranked.len(), 1);
        assert!(
            !ranked[0].matched_indices.is_empty(),
            "expected nucleo to mark at least one matched index",
        );
        let mut sorted = ranked[0].matched_indices.clone();
        sorted.sort_unstable();
        assert_eq!(ranked[0].matched_indices, sorted);
    }
}
