use crate::{
    item::ItemHandle,
    review_item,
    review_session::{ReviewApplyResult, ReviewSession, ReviewSessionEvent},
    status_bar::StatusItemView,
    theme::ActiveTheme,
};
use gpui::{
    div, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window,
};
use stoat::review_session::ReviewProgress as InnerProgress;

/// Status-bar segment that surfaces the workspace's active review
/// session, independent of which pane is focused. The workspace pushes
/// the session to display via [`Self::set_review_session`] whenever the
/// open-review set changes -- the focused review when one is focused,
/// otherwise the first open review -- so the segment persists while a
/// review is open in any pane and clears once the last one closes.
///
/// Renders an optional `[mode]` comparison-mode prefix (Unstaged /
/// Staged / All, absent for non-working-tree reviews) followed by the
/// `{staged}/{total}` progress, or a recorded
/// [`stoat_action::ReviewApplyStaged`] outcome (` applied N ` /
/// ` applied N/M `). Stays visible even at zero chunks; hides only when
/// no session is bound.
///
/// Subscribes to the bound session's [`ReviewSessionEvent`]s so
/// chunk-staging and refreshes update the counters without polling. The
/// `REV` mode badge stays tied to the focused pane's mode and is a
/// separate item.
pub struct ReviewProgress {
    progress: Option<InnerProgress>,
    apply_result: Option<ReviewApplyResult>,
    comparison: Option<&'static str>,
    session: Option<WeakEntity<ReviewSession>>,
    _session_subscription: Option<Subscription>,
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
            comparison: None,
            session: None,
            _session_subscription: None,
        }
    }

    pub fn progress(&self) -> Option<&InnerProgress> {
        self.progress.as_ref()
    }

    pub fn apply_result(&self) -> Option<&ReviewApplyResult> {
        self.apply_result.as_ref()
    }

    pub fn comparison(&self) -> Option<&'static str> {
        self.comparison
    }

    /// Bind the segment to `session` (or clear it with `None`). The
    /// workspace calls this on every pane change with the review to
    /// display. Skips re-binding when the same session is pushed again
    /// so the live subscription is not churned.
    pub fn set_review_session(
        &mut self,
        session: Option<Entity<ReviewSession>>,
        cx: &mut Context<'_, Self>,
    ) {
        let current = self.session.as_ref().map(|s| s.entity_id());
        let next = session.as_ref().map(|s| s.entity_id());
        if current == next {
            return;
        }
        match session {
            Some(session) => self.bind_to_session(&session, cx),
            None => self.clear(cx),
        }
    }

    fn bind_to_session(&mut self, session: &Entity<ReviewSession>, cx: &mut Context<'_, Self>) {
        self.session = Some(session.downgrade());
        self._session_subscription = Some(cx.subscribe(
            session,
            |this, session, _event: &ReviewSessionEvent, cx| {
                this.refresh_from_session(&session, cx);
            },
        ));
        self.refresh_from_session(session, cx);
    }

    fn refresh_from_session(
        &mut self,
        session: &Entity<ReviewSession>,
        cx: &mut Context<'_, Self>,
    ) {
        let session = session.read(cx);
        let next_progress = Some(session.progress());
        let next_apply = session.last_apply_result().cloned();
        let next_comparison = review_item::comparison_mode_label(&session.inner().source);
        if self.progress != next_progress
            || self.apply_result != next_apply
            || self.comparison != next_comparison
        {
            self.progress = next_progress;
            self.apply_result = next_apply;
            self.comparison = next_comparison;
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.progress.is_none()
            && self.apply_result.is_none()
            && self.comparison.is_none()
            && self.session.is_none()
        {
            return;
        }
        self.progress = None;
        self.apply_result = None;
        self.comparison = None;
        self.session = None;
        self._session_subscription = None;
        cx.notify();
    }
}

impl Render for ReviewProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = format_segment(
            self.comparison,
            self.apply_result.as_ref(),
            self.progress.as_ref(),
        )
        .map(|text| {
            div()
                .px_2()
                .text_color(cx.theme().statusbar_text)
                .child(SharedString::from(text))
        });
        div().children(label)
    }
}

impl StatusItemView for ReviewProgress {
    /// The segment is driven workspace-wide via
    /// [`Self::set_review_session`], not by the focused pane item, so
    /// this fan-out callback is intentionally inert (mirroring
    /// [`crate::status_bar::mode_badge::ModeBadge`]).
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _cx: &mut Context<'_, Self>,
    ) {
    }
}

/// Build the segment label: an optional `[mode]` comparison-mode prefix
/// followed by the apply-result or live-progress body. Returns `None`
/// only when no session is bound (`progress` is `None`), which hides the
/// segment; a bound session with zero chunks still renders.
fn format_segment(
    comparison: Option<&str>,
    apply: Option<&ReviewApplyResult>,
    progress: Option<&InnerProgress>,
) -> Option<String> {
    let progress = progress?;
    let body = match apply {
        Some(apply) => format_apply_result(apply),
        None => format_progress(progress),
    };
    Some(match comparison {
        Some(mode) => format!(" [{mode}]{body}"),
        None => body,
    })
}

fn format_progress(progress: &InnerProgress) -> String {
    let mut out = format!(" {}/{}", progress.staged, progress.total);
    if progress.approved > 0 {
        out.push_str(&format!(" approved:{}", progress.approved));
    }
    if progress.skipped > 0 {
        out.push_str(&format!(" skip:{}", progress.skipped));
    }
    out.push(' ');
    out
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
    use crate::review_session::ReviewSession;
    use gpui::{AppContext, TestAppContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::review_session::{ReviewSession as InnerSession, ReviewSource};

    fn new_badge(cx: &mut TestAppContext) -> Entity<ReviewProgress> {
        cx.update(|cx| cx.new(|_| ReviewProgress::new()))
    }

    fn new_session(cx: &mut TestAppContext, source: ReviewSource) -> Entity<ReviewSession> {
        cx.update(|cx| cx.new(|_| ReviewSession::new(InnerSession::new(source))))
    }

    fn working_tree(cx: &mut TestAppContext) -> Entity<ReviewSession> {
        new_session(
            cx,
            ReviewSource::WorkingTree {
                workdir: PathBuf::from("/repo"),
            },
        )
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        badge.read_with(&cx, |b, _| {
            assert!(b.progress().is_none());
            assert_eq!(b.comparison(), None);
        });
    }

    #[test]
    fn binds_session_shows_comparison_and_is_visible_at_zero_chunks() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        let session = working_tree(&mut cx);
        badge.update(&mut cx, |b, cx| b.set_review_session(Some(session), cx));
        badge.read_with(&cx, |b, _| {
            assert_eq!(b.comparison(), Some("All"));
            assert_eq!(b.progress().map(|p| p.total), Some(0));
        });
    }

    #[test]
    fn non_working_tree_review_has_no_comparison_mode() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        let session = new_session(
            &mut cx,
            ReviewSource::InMemory {
                files: Arc::new(Vec::new()),
            },
        );
        badge.update(&mut cx, |b, cx| b.set_review_session(Some(session), cx));
        badge.read_with(&cx, |b, _| {
            assert_eq!(b.comparison(), None);
            assert!(b.progress().is_some());
        });
    }

    #[test]
    fn set_review_session_none_clears() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        let session = working_tree(&mut cx);
        badge.update(&mut cx, |b, cx| b.set_review_session(Some(session), cx));
        badge.update(&mut cx, |b, cx| b.set_review_session(None, cx));
        badge.read_with(&cx, |b, _| {
            assert!(b.progress().is_none());
            assert_eq!(b.comparison(), None);
        });
    }

    #[test]
    fn set_active_pane_item_is_noop() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        let session = working_tree(&mut cx);
        badge.update(&mut cx, |b, cx| b.set_review_session(Some(session), cx));
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(None, cx));
        badge.read_with(&cx, |b, _| {
            assert_eq!(b.comparison(), Some("All"), "fan-out must not unbind");
        });
    }

    #[test]
    fn session_event_refreshes_segment() {
        let mut cx = TestAppContext::single();
        let badge = new_badge(&mut cx);
        let session = working_tree(&mut cx);
        badge.update(&mut cx, |b, cx| {
            b.set_review_session(Some(session.clone()), cx)
        });
        badge.read_with(&cx, |b, _| assert!(b.apply_result().is_none()));

        session.update(&mut cx, |s, cx| {
            s.set_apply_result(
                ReviewApplyResult {
                    applied: 2,
                    total: 2,
                    first_failure: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        badge.read_with(&cx, |b, _| {
            assert_eq!(
                b.apply_result().map(|r| r.applied),
                Some(2),
                "session event refreshes the bound segment",
            );
        });
    }

    #[test]
    fn format_segment_prefixes_comparison_mode() {
        let progress = InnerProgress {
            staged: 2,
            total: 5,
            ..Default::default()
        };
        assert_eq!(
            format_segment(Some("All"), None, Some(&progress)),
            Some(" [All] 2/5 ".to_string()),
        );
    }

    #[test]
    fn format_segment_without_comparison_is_progress_only() {
        let progress = InnerProgress {
            staged: 2,
            total: 5,
            ..Default::default()
        };
        assert_eq!(
            format_segment(None, None, Some(&progress)),
            Some(" 2/5 ".to_string()),
        );
    }

    #[test]
    fn format_segment_visible_at_zero_total() {
        let progress = InnerProgress {
            staged: 0,
            total: 0,
            ..Default::default()
        };
        assert_eq!(
            format_segment(Some("Unstaged"), None, Some(&progress)),
            Some(" [Unstaged] 0/0 ".to_string()),
        );
    }

    #[test]
    fn format_segment_prefers_apply_result() {
        let result = ReviewApplyResult {
            applied: 3,
            total: 3,
            first_failure: None,
        };
        let progress = InnerProgress {
            staged: 0,
            total: 5,
            ..Default::default()
        };
        assert_eq!(
            format_segment(Some("All"), Some(&result), Some(&progress)),
            Some(" [All] applied 3 ".to_string()),
        );
    }

    #[test]
    fn format_segment_hidden_without_progress() {
        assert_eq!(format_segment(Some("All"), None, None), None);
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
    fn format_label_shows_approved_when_present() {
        let progress = InnerProgress {
            staged: 1,
            total: 4,
            approved: 2,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 1/4 approved:2 ");
    }

    #[test]
    fn format_label_hides_approved_when_zero() {
        let progress = InnerProgress {
            staged: 1,
            total: 4,
            approved: 0,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 1/4 ");
    }

    #[test]
    fn format_label_shows_approved_before_skip() {
        let progress = InnerProgress {
            staged: 1,
            total: 5,
            approved: 3,
            skipped: 1,
            ..Default::default()
        };
        assert_eq!(format_progress(&progress), " 1/5 approved:3 skip:1 ");
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
}
