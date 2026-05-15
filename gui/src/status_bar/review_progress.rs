use crate::{
    editor::{Editor, EditorEvent},
    item::ItemHandle,
    review_session::ReviewApplyResult,
    status_bar::StatusItemView,
    theme::statusbar_text_color,
};
use gpui::{
    div, App, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window,
};
use stoat::review_session::ReviewProgress as InnerProgress;

/// Status-bar item that surfaces the active editor's review session
/// state. When the session has recorded a
/// [`stoat_action::ReviewApplyStaged`] outcome via
/// [`crate::review_session::ReviewSession::set_apply_result`] the
/// badge renders that result (` applied N ` or
/// ` applied N/M `); otherwise it falls back to the live
/// `{staged}/{total}` progress, plus a ` skip:{skipped} ` segment
/// when any chunks have been skipped. Hides entirely when the
/// active item is not an editor, has no review session attached,
/// or has a review session with zero chunks and no apply result.
///
/// Rebinds whenever the active pane item changes; subscribes to the
/// editor's [`EditorEvent::Changed`] so review-session mutations --
/// which the editor re-emits via its
/// [`crate::review_session::ReviewSessionEvent`] subscription --
/// refresh the badge without polling.
pub struct ReviewProgress {
    progress: Option<InnerProgress>,
    apply_result: Option<ReviewApplyResult>,
    editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
}

impl Default for ReviewProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewProgress {
    pub fn new() -> Self {
        Self {
            progress: None,
            apply_result: None,
            editor: None,
            _editor_subscription: None,
        }
    }

    pub fn progress(&self) -> Option<&InnerProgress> {
        self.progress.as_ref()
    }

    pub fn apply_result(&self) -> Option<&ReviewApplyResult> {
        self.apply_result.as_ref()
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
        let next_progress = compute_progress(editor.read(cx), cx);
        let next_apply = compute_apply_result(editor.read(cx), cx);
        if self.progress != next_progress || self.apply_result != next_apply {
            self.progress = next_progress;
            self.apply_result = next_apply;
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.progress.is_none()
            && self.apply_result.is_none()
            && self.editor.is_none()
            && self._editor_subscription.is_none()
        {
            return;
        }
        self.progress = None;
        self.apply_result = None;
        self.editor = None;
        self._editor_subscription = None;
        cx.notify();
    }
}

impl Render for ReviewProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = format_badge(self.apply_result.as_ref(), self.progress.as_ref()).map(|text| {
            div()
                .px_2()
                .text_color(statusbar_text_color(cx))
                .child(SharedString::from(text))
        });
        div().children(label)
    }
}

impl StatusItemView for ReviewProgress {
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

fn compute_progress(editor: &Editor, cx: &App) -> Option<InnerProgress> {
    let session = editor.review_session()?;
    let progress = session.read(cx).progress();
    if progress.total == 0 {
        return None;
    }
    Some(progress)
}

fn compute_apply_result(editor: &Editor, cx: &App) -> Option<ReviewApplyResult> {
    let session = editor.review_session()?;
    session.read(cx).last_apply_result().cloned()
}

/// Render the badge label, preferring an apply result over the live
/// progress counters when both are present. Returns `None` when
/// neither piece of state should produce a visible badge.
fn format_badge(
    apply: Option<&ReviewApplyResult>,
    progress: Option<&InnerProgress>,
) -> Option<String> {
    if let Some(apply) = apply {
        return Some(format_apply_result(apply));
    }
    progress.map(format_progress)
}

fn format_progress(progress: &InnerProgress) -> String {
    if progress.skipped > 0 {
        format!(
            " {}/{} skip:{} ",
            progress.staged, progress.total, progress.skipped
        )
    } else {
        format!(" {}/{} ", progress.staged, progress.total)
    }
}

fn format_apply_result(result: &ReviewApplyResult) -> String {
    if result.first_failure.is_some() {
        format!(" applied {}/{} ", result.applied, result.total)
    } else {
        format!(" applied {} ", result.applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::{Editor, EditorMode},
        globals::ExecutorGlobal,
        multi_buffer::MultiBuffer,
        review_session::ReviewSession,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::{
        buffer::BufferId,
        review_session::{ReviewSession as InnerSession, ReviewSource},
    };
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

    fn new_review_session(cx: &mut TestAppContext) -> Entity<ReviewSession> {
        cx.update(|cx| {
            cx.new(|_| {
                ReviewSession::new(InnerSession::new(ReviewSource::InMemory {
                    files: Arc::new(Vec::new()),
                }))
            })
        })
    }

    fn new_badge(cx: &mut TestAppContext) -> Entity<ReviewProgress> {
        cx.update(|cx| cx.new(|_| ReviewProgress::new()))
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let badge = new_badge(&mut cx);
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn editor_without_review_session_yields_no_progress() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn editor_with_empty_review_session_yields_no_progress() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let session = new_review_session(&mut cx);
        editor.update(&mut cx, |ed, cx| ed.set_review_session(Some(session), cx));
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn clear_drops_progress_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(None, cx));
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn rebinding_swaps_editor() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor_a = new_editor(&mut cx);
        let editor_b = new_editor(&mut cx);
        let badge = new_badge(&mut cx);
        let handle_a: Box<dyn ItemHandle> = Box::new(editor_a);
        let handle_b: Box<dyn ItemHandle> = Box::new(editor_b);
        badge.update(&mut cx, |b, cx| {
            b.set_active_pane_item(Some(&*handle_a), cx)
        });
        badge.update(&mut cx, |b, cx| {
            b.set_active_pane_item(Some(&*handle_b), cx)
        });
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn review_session_change_propagates_through_editor_event() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx);
        let session = new_review_session(&mut cx);
        editor.update(&mut cx, |ed, cx| {
            ed.set_review_session(Some(session.clone()), cx)
        });
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));

        session.update(&mut cx, |s, cx| s.notify_changed(cx));
        cx.run_until_parked();
        badge.read_with(&cx, |b, _| assert!(b.progress().is_none()));
    }

    #[test]
    fn format_label_hides_skip_when_zero() {
        let progress = InnerProgress {
            staged: 2,
            total: 5,
            skipped: 0,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 2/5 ");
    }

    #[test]
    fn format_label_shows_skip_when_present() {
        let progress = InnerProgress {
            staged: 1,
            total: 4,
            skipped: 2,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 1/4 skip:2 ");
    }

    #[test]
    fn format_label_zero_staged_no_skip() {
        let progress = InnerProgress {
            staged: 0,
            total: 3,
            skipped: 0,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 0/3 ");
    }

    #[test]
    fn format_apply_result_renders_just_applied_count_on_full_success() {
        let result = ReviewApplyResult {
            applied: 3,
            total: 3,
            first_failure: None,
        };
        assert_eq!(format_apply_result(&result), " applied 3 ");
    }

    #[test]
    fn format_apply_result_renders_partial_count_on_failure() {
        let result = ReviewApplyResult {
            applied: 1,
            total: 3,
            first_failure: Some("disk full".to_string()),
        };
        assert_eq!(format_apply_result(&result), " applied 1/3 ");
    }

    #[test]
    fn format_badge_prefers_apply_result_over_progress() {
        let result = ReviewApplyResult {
            applied: 2,
            total: 2,
            first_failure: None,
        };
        let progress = InnerProgress {
            staged: 0,
            total: 5,
            ..Default::default()
        };
        assert_eq!(
            format_badge(Some(&result), Some(&progress)),
            Some(" applied 2 ".to_string()),
        );
    }

    #[test]
    fn format_badge_falls_back_to_progress_when_no_apply_result() {
        let progress = InnerProgress {
            staged: 2,
            total: 5,
            ..Default::default()
        };
        assert_eq!(
            format_badge(None, Some(&progress)),
            Some(" 2/5 ".to_string()),
        );
    }

    #[test]
    fn format_badge_is_none_when_neither_set() {
        assert_eq!(format_badge(None, None), None);
    }
}
