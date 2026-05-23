mod delegate;

use crate::{
    editor::{Editor, EditorEvent},
    modal_layer::ModalView,
};
pub use delegate::{PickerDelegate, PickerSecondary};
use gpui::{
    div, uniform_list, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, UniformListScrollHandle, Window,
};
use std::ops::Range;
pub use stoat::fuzzy::{match_and_rank, RankedMatch};
use stoat_action::ActionKind;

/// Action kinds whose handlers open a top-level picker modal in
/// the GUI. Used by [`Workspace::dispatch_action`]'s post-dispatch
/// tail to record the most recently opened picker for
/// [`stoat_action::OpenLastPicker`] recall, and by the recall
/// handler itself to validate a candidate before re-dispatching.
///
/// The set covers every picker wired through `dispatch_action`'s
/// match arms; future picker items extend it as they land.
pub(crate) fn is_picker_open_kind(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::OpenCommandPalette
            | ActionKind::OpenFileFinder
            | ActionKind::OpenBufferPicker
            | ActionKind::OpenSymbolPicker
            | ActionKind::OpenWorkspaceSymbolPicker
            | ActionKind::OpenDiagnosticsPicker
            | ActionKind::OpenWorkspaceDiagnosticsPicker
            | ActionKind::OpenJumplistPicker
            | ActionKind::OpenGlobalSearch
            | ActionKind::OpenWorkspacePicker
            | ActionKind::OpenCheckpointPicker
    )
}

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

/// Convert nucleo-style character indices into byte-range
/// highlights ready to feed into [`gpui::StyledText::with_highlights`].
///
/// `char_indices` are the matched cell positions as
/// [`RankedMatch::matched_indices`] returns them: sorted,
/// deduplicated, in haystack character (UTF-32) coordinates.
/// Contiguous indices collapse into a single range so the returned
/// `Vec` carries one entry per run, not one per cell. Indices past
/// the haystack's character count are dropped silently so a stale
/// index list (e.g. carried over a buffer mutation) cannot panic the
/// renderer.
///
/// The returned ranges are byte offsets aligned to UTF-8 character
/// boundaries, so [`gpui::StyledText::with_highlights`] accepts
/// them without further conversion. The caller chooses `style`;
/// typical picker delegates resolve a foreground color from the
/// active [`stoat::theme::Theme`] (e.g. via the
/// `UI_SEARCH_MATCH` scope) and supply a [`HighlightStyle`] whose
/// `color` field is set.
pub fn match_highlight_runs(
    haystack: &str,
    char_indices: &[u32],
    style: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if char_indices.is_empty() || haystack.is_empty() {
        return Vec::new();
    }

    let mut char_to_byte: Vec<usize> = Vec::with_capacity(haystack.len() + 1);
    char_to_byte.push(0);
    for (byte_pos, _) in haystack.char_indices() {
        if byte_pos != 0 {
            char_to_byte.push(byte_pos);
        }
    }
    char_to_byte.push(haystack.len());

    let total_chars = char_to_byte.len() - 1;
    let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut i = 0;
    while i < char_indices.len() {
        let start_char = char_indices[i] as usize;
        if start_char >= total_chars {
            break;
        }
        let mut end_char = start_char + 1;
        let mut j = i + 1;
        while j < char_indices.len()
            && (char_indices[j] as usize) == end_char
            && end_char < total_chars
        {
            end_char += 1;
            j += 1;
        }
        let start_byte = char_to_byte[start_char];
        let end_byte = char_to_byte[end_char];
        runs.push((start_byte..end_byte, style));
        i = j;
    }

    runs
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
    pub fn new(mut delegate: D, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let query_editor = cx.new(|cx| Editor::single_line(window, cx));
        delegate.on_attach(&query_editor);
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
    /// into the picker. Returns `true` when the action was handled
    /// (selection move, confirm, dismiss) so the caller can
    /// short-circuit its own dispatch; returns `false` for any
    /// action the picker does not own.
    ///
    /// Dismiss is routed through [`ActionKind::DismissModal`]: the
    /// picker calls [`PickerDelegate::dismissed`] and then emits
    /// [`DismissEvent`] so the [`ModalLayer`]'s existing dismissal
    /// subscription pops the modal from the stack.
    pub fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.delegate.handle_action(action, window, cx) {
            return true;
        }
        match action.kind() {
            ActionKind::PickerSelectPrev
            | ActionKind::PaletteSelectPrev
            | ActionKind::FileFinderSelectPrev => {
                self.move_selection(-1, cx);
                true
            },
            ActionKind::PickerSelectNext
            | ActionKind::PaletteSelectNext
            | ActionKind::FileFinderSelectNext => {
                self.move_selection(1, cx);
                true
            },
            ActionKind::PickerConfirm => self.confirm_selection(None, window, cx),
            ActionKind::PickerConfirmSplitRight => {
                self.confirm_selection(Some(PickerSecondary::OpenInRight), window, cx)
            },
            ActionKind::PickerConfirmSplitDown => {
                self.confirm_selection(Some(PickerSecondary::OpenInDown), window, cx)
            },
            ActionKind::DismissModal => {
                self.delegate.dismissed(cx);
                cx.emit(DismissEvent);
                true
            },
            _ => false,
        }
    }

    fn move_selection(&mut self, delta: i32, cx: &mut Context<'_, Self>) {
        let count = self.delegate.match_count();
        if count == 0 {
            return;
        }
        let current = self.delegate.selected_index() as i64;
        let last = count as i64 - 1;
        let next = (current + delta as i64).clamp(0, last) as usize;
        if next != self.delegate.selected_index() {
            self.delegate.set_selected_index(next, cx);
            cx.notify();
        }
    }

    fn confirm_selection(
        &mut self,
        secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.delegate.match_count() == 0 {
            return true;
        }
        self.delegate.confirm(secondary, window, cx);
        true
    }
}

impl<D: PickerDelegate> ModalView for Picker<D> {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        Picker::handle_action(self, action, window, cx)
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.confirm_selection(None, window, cx)
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.delegate.dismissed(cx);
        cx.emit(DismissEvent);
        true
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
            _window: &mut Window,
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
    fn handle_action_returns_false_for_non_picker_kinds() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let picker = h.picker.clone();
        let handled = h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                let action = crate::actions::SetActivePane { pane_id: 0 };
                p.handle_action(&action, window, cx)
            })
        });
        assert!(!handled, "non-picker action should fall through");
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 0);
    }

    #[test]
    fn handle_action_select_next_advances_selection() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into(), "gamma".into()]);
        let picker = h.picker.clone();
        let handled = h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerSelectNext, window, cx)
            })
        });
        assert!(handled);
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 1);
    }

    #[test]
    fn handle_action_palette_and_finder_select_aliases_move_selection() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into(), "gamma".into()]);
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                assert!(p.handle_action(&stoat_action::PaletteSelectNext, window, cx));
                assert!(p.handle_action(&stoat_action::FileFinderSelectNext, window, cx));
            });
        });
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 2);

        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                assert!(p.handle_action(&stoat_action::FileFinderSelectPrev, window, cx));
                assert!(p.handle_action(&stoat_action::PaletteSelectPrev, window, cx));
            });
        });
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 0);
    }

    #[test]
    fn handle_action_select_next_clamps_at_last_row() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into()]);
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerSelectNext, window, cx);
                p.handle_action(&stoat_action::PickerSelectNext, window, cx);
                p.handle_action(&stoat_action::PickerSelectNext, window, cx)
            })
        });
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 1);
    }

    #[test]
    fn handle_action_select_prev_clamps_at_first_row() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into(), "beta".into()]);
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerSelectPrev, window, cx)
            })
        });
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 0);
    }

    #[test]
    fn handle_action_select_next_on_empty_is_noop() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, Vec::new());
        let picker = h.picker.clone();
        let handled = h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerSelectNext, window, cx)
            })
        });
        assert!(handled);
        assert_eq!(h.picker.read_with(h.vcx, |p, _| p.selected_index()), 0);
    }

    #[test]
    fn handle_action_confirm_passes_no_secondary() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let confirmed = h
            .picker
            .read_with(h.vcx, |p, _| p.delegate().confirmed.clone());
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx)
            })
        });
        let snapshot = confirmed.lock().expect("confirmed mutex").clone();
        assert_eq!(snapshot, vec![None]);
    }

    #[test]
    fn handle_action_confirm_split_right_passes_open_in_right() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let confirmed = h
            .picker
            .read_with(h.vcx, |p, _| p.delegate().confirmed.clone());
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirmSplitRight, window, cx)
            })
        });
        let snapshot = confirmed.lock().expect("confirmed mutex").clone();
        assert_eq!(snapshot, vec![Some(PickerSecondary::OpenInRight)]);
    }

    #[test]
    fn handle_action_confirm_split_down_passes_open_in_down() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let confirmed = h
            .picker
            .read_with(h.vcx, |p, _| p.delegate().confirmed.clone());
        let picker = h.picker.clone();
        h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirmSplitDown, window, cx)
            })
        });
        let snapshot = confirmed.lock().expect("confirmed mutex").clone();
        assert_eq!(snapshot, vec![Some(PickerSecondary::OpenInDown)]);
    }

    #[test]
    fn handle_action_confirm_on_empty_does_not_fire_delegate() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, Vec::new());
        let confirmed = h
            .picker
            .read_with(h.vcx, |p, _| p.delegate().confirmed.clone());
        let picker = h.picker.clone();
        let handled = h.vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx)
            })
        });
        assert!(handled);
        let snapshot = confirmed.lock().expect("confirmed mutex").clone();
        assert!(snapshot.is_empty(), "delegate confirm should not fire");
    }

    #[test]
    fn handle_action_dismiss_calls_delegate_and_emits_dismiss_event() {
        let mut cx = TestAppContext::single();
        let h = new_picker(&mut cx, vec!["alpha".into()]);
        let dismissed = h
            .picker
            .read_with(h.vcx, |p, _| p.delegate().dismissed.clone());
        let dismiss_events: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let picker = h.picker.clone();
        let _subscription = h.vcx.update(|_, cx| {
            let sink = dismiss_events.clone();
            cx.subscribe(&picker, move |_, _: &DismissEvent, _| {
                *sink.lock().expect("dismiss events mutex") += 1;
            })
        });
        let picker_for_call = picker.clone();
        let handled = h.vcx.update(|window, cx| {
            picker_for_call.update(cx, |p, cx| {
                p.handle_action(&stoat_action::DismissModal, window, cx)
            })
        });
        h.vcx.run_until_parked();
        assert!(handled);
        assert_eq!(*dismissed.lock().expect("dismissed mutex"), 1);
        assert_eq!(*dismiss_events.lock().expect("dismiss events mutex"), 1);
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

    fn style() -> HighlightStyle {
        HighlightStyle {
            color: Some(gpui::Hsla {
                h: 0.0,
                s: 0.0,
                l: 1.0,
                a: 1.0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn match_highlight_runs_empty_inputs_return_empty() {
        let s = style();
        assert!(match_highlight_runs("", &[], s).is_empty());
        assert!(match_highlight_runs("hello", &[], s).is_empty());
        assert!(match_highlight_runs("", &[0, 1], s).is_empty());
    }

    #[test]
    fn match_highlight_runs_merges_contiguous_indices() {
        let s = style();
        let runs = match_highlight_runs("foo.rs", &[0, 1, 2], s);
        assert_eq!(runs, vec![(0..3, s)]);
    }

    #[test]
    fn match_highlight_runs_splits_discontiguous_indices() {
        let s = style();
        let runs = match_highlight_runs("barfoo", &[0, 3, 4, 5], s);
        assert_eq!(runs, vec![(0..1, s), (3..6, s)]);
    }

    #[test]
    fn match_highlight_runs_drops_indices_past_end() {
        let s = style();
        let runs = match_highlight_runs("foo", &[1, 2, 5, 7], s);
        assert_eq!(runs, vec![(1..3, s)]);
    }

    #[test]
    fn match_highlight_runs_returns_byte_ranges_for_multibyte_chars() {
        let s = style();
        let runs = match_highlight_runs("café", &[2, 3], s);
        assert_eq!(runs, vec![(2..5, s)]);
    }

    #[test]
    fn match_highlight_runs_handles_match_spanning_multibyte_boundary() {
        let s = style();
        let runs = match_highlight_runs("a\u{00e9}b", &[0, 1, 2], s);
        assert_eq!(runs, vec![(0..4, s)]);
    }

    #[test]
    fn match_highlight_runs_index_at_last_char_produces_run() {
        let s = style();
        let runs = match_highlight_runs("abc", &[2], s);
        assert_eq!(runs, vec![(2..3, s)]);
    }
}
