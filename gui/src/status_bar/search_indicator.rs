use crate::{
    editor::{search::SearchState, Editor, EditorEvent},
    item::ItemHandle,
    status_bar::StatusItemView,
    theme::ActiveTheme,
};
use gpui::{
    div, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window,
};

/// Status-bar item that surfaces the active editor's in-buffer
/// search query, prefixed with `/` for forward search or `?` for
/// reverse. Hides entirely when the active item is not an editor or
/// the editor has no search state.
///
/// Rebinds whenever the active pane item changes; subscribes to the
/// editor's [`EditorEvent::Changed`] so the indicator refreshes when
/// the search state is set or cleared.
pub struct SearchQueryIndicator {
    state: Option<SearchState>,
    editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
}

impl Default for SearchQueryIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchQueryIndicator {
    pub fn new() -> Self {
        Self {
            state: None,
            editor: None,
            _editor_subscription: None,
        }
    }

    pub fn state(&self) -> Option<&SearchState> {
        self.state.as_ref()
    }

    fn bind_to_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        self.editor = Some(editor.downgrade());
        self._editor_subscription = Some(cx.subscribe(
            editor,
            |this, editor, _event: &EditorEvent, cx| {
                this.refresh_from_editor(&editor, cx);
            },
        ));
        self.refresh_from_editor(editor, cx);
    }

    fn refresh_from_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let next = compute_state(editor.read(cx));
        if self.state != next {
            self.state = next;
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.state.is_none() && self.editor.is_none() && self._editor_subscription.is_none() {
            return;
        }
        self.state = None;
        self.editor = None;
        self._editor_subscription = None;
        cx.notify();
    }
}

impl Render for SearchQueryIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = self.state.as_ref().map(|state| {
            div()
                .px_2()
                .text_color(cx.theme().statusbar_text)
                .child(SharedString::from(format_label(state)))
        });
        div().children(label)
    }
}

impl StatusItemView for SearchQueryIndicator {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<'_, Self>,
    ) {
        let editor = active_pane_item.and_then(|item| item.to_any_view().downcast::<Editor>().ok());
        match editor {
            Some(editor) => self.bind_to_editor(&editor, cx),
            None => self.clear(cx),
        }
    }
}

fn compute_state(editor: &Editor) -> Option<SearchState> {
    let state = editor.search_state()?;
    if state.query().is_empty() {
        return None;
    }
    Some(state.clone())
}

fn format_label(state: &SearchState) -> String {
    format!(" {}{} ", state.direction().prefix(), state.query())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::{search::SearchDirection, Editor, EditorMode},
        globals::ExecutorGlobal,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_editor(cx: &mut TestAppContext) -> Entity<Editor> {
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), ""));
            let multi_buffer = {
                let buffer = buffer.clone();
                cx.new(|cx| MultiBuffer::singleton(buffer, cx))
            };
            let executor = cx.global::<ExecutorGlobal>().0.clone();
            let display_map = {
                let buffer = buffer.clone();
                cx.new(|cx| DisplayMap::new(buffer, executor, cx))
            };
            let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn new_indicator(cx: &mut TestAppContext) -> Entity<SearchQueryIndicator> {
        cx.update(|cx| cx.new(|_| SearchQueryIndicator::new()))
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let indicator = new_indicator(&mut cx);
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));
    }

    #[test]
    fn editor_without_search_state_yields_no_label() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let indicator = new_indicator(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));
    }

    #[test]
    fn editor_with_forward_search_state_shows_state() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        editor.update(&mut cx, |ed, cx| {
            ed.set_search_state(Some(SearchState::new("foo", SearchDirection::Forward)), cx)
        });
        let indicator = new_indicator(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        indicator.read_with(&cx, |i, _| {
            let state = i.state().expect("state");
            assert_eq!(state.query(), "foo");
            assert_eq!(state.direction(), SearchDirection::Forward);
        });
    }

    #[test]
    fn empty_query_yields_no_state() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        editor.update(&mut cx, |ed, cx| {
            ed.set_search_state(Some(SearchState::new("", SearchDirection::Forward)), cx)
        });
        let indicator = new_indicator(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));
    }

    #[test]
    fn rebinding_swaps_state() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let first = new_editor(&mut cx);
        let second = new_editor(&mut cx);
        first.update(&mut cx, |ed, cx| {
            ed.set_search_state(
                Some(SearchState::new("first", SearchDirection::Forward)),
                cx,
            )
        });
        second.update(&mut cx, |ed, cx| {
            ed.set_search_state(
                Some(SearchState::new("second", SearchDirection::Reverse)),
                cx,
            )
        });
        let indicator = new_indicator(&mut cx);
        let handle_first: Box<dyn ItemHandle> = Box::new(first);
        let handle_second: Box<dyn ItemHandle> = Box::new(second);
        indicator.update(&mut cx, |i, cx| {
            i.set_active_pane_item(Some(&*handle_first), cx)
        });
        indicator.update(&mut cx, |i, cx| {
            i.set_active_pane_item(Some(&*handle_second), cx)
        });
        indicator.read_with(&cx, |i, _| {
            let state = i.state().expect("state");
            assert_eq!(state.query(), "second");
            assert_eq!(state.direction(), SearchDirection::Reverse);
        });
    }

    #[test]
    fn clear_drops_state_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        editor.update(&mut cx, |ed, cx| {
            ed.set_search_state(Some(SearchState::new("q", SearchDirection::Forward)), cx)
        });
        let indicator = new_indicator(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(None, cx));
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));
    }

    #[test]
    fn editor_search_state_change_propagates() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let indicator = new_indicator(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor.clone());
        indicator.update(&mut cx, |i, cx| i.set_active_pane_item(Some(&*handle), cx));
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));

        editor.update(&mut cx, |ed, cx| {
            ed.set_search_state(
                Some(SearchState::new("hello", SearchDirection::Reverse)),
                cx,
            )
        });
        cx.run_until_parked();
        indicator.read_with(&cx, |i, _| {
            let state = i.state().expect("state");
            assert_eq!(state.query(), "hello");
            assert_eq!(state.direction(), SearchDirection::Reverse);
        });

        editor.update(&mut cx, |ed, cx| ed.set_search_state(None, cx));
        cx.run_until_parked();
        indicator.read_with(&cx, |i, _| assert!(i.state().is_none()));
    }

    #[test]
    fn format_label_forward() {
        let state = SearchState::new("needle", SearchDirection::Forward);
        assert_eq!(format_label(&state), " /needle ");
    }

    #[test]
    fn format_label_reverse() {
        let state = SearchState::new("needle", SearchDirection::Reverse);
        assert_eq!(format_label(&state), " ?needle ");
    }
}
