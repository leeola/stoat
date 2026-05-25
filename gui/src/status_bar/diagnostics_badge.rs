use crate::{
    editor::{Editor, EditorEvent},
    item::ItemHandle,
    status_bar::StatusItemView,
    theme::ActiveTheme,
};
use gpui::{
    div, App, Context, Entity, Hsla, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window,
};
use lsp_types::DiagnosticSeverity;
use stoat::diagnostics::DiagnosticSummary;

/// Status-bar item that surfaces the active editor's
/// diagnostic counts as ` E{e} W{w} I{i} H{h} `, omitting severities
/// whose count is zero. The label's foreground matches the worst
/// severity present so the row reads at a glance. Hides entirely
/// when the active item is not an editor, has no path, has no
/// attached [`DiagnosticSet`], or has no diagnostics for its path.
///
/// Rebinds whenever the active pane item changes; subscribes to the
/// editor's [`EditorEvent::Changed`] so diagnostic publishes -- which
/// the editor re-emits as `Changed` via its
/// [`DiagnosticSet`] subscription -- refresh the badge without
/// polling.
///
/// [`DiagnosticSet`]: crate::diagnostics::DiagnosticSet
pub struct DiagnosticsBadge {
    summary: Option<(DiagnosticSummary, DiagnosticSeverity)>,
    editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
}

impl Default for DiagnosticsBadge {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticsBadge {
    pub fn new() -> Self {
        Self {
            summary: None,
            editor: None,
            _editor_subscription: None,
        }
    }

    pub fn summary(&self) -> Option<&(DiagnosticSummary, DiagnosticSeverity)> {
        self.summary.as_ref()
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
        let next = compute_summary(editor.read(cx), cx);
        if self.summary != next {
            self.summary = next;
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.summary.is_none() && self.editor.is_none() && self._editor_subscription.is_none() {
            return;
        }
        self.summary = None;
        self.editor = None;
        self._editor_subscription = None;
        cx.notify();
    }
}

impl Render for DiagnosticsBadge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label = self.summary.as_ref().map(|(summary, worst)| {
            div()
                .px_2()
                .text_color(severity_color(*worst, cx))
                .child(SharedString::from(format_label(summary)))
        });
        div().children(label)
    }
}

impl StatusItemView for DiagnosticsBadge {
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

fn compute_summary(editor: &Editor, cx: &App) -> Option<(DiagnosticSummary, DiagnosticSeverity)> {
    let path = editor.file_path()?;
    let set = editor.diagnostic_set()?;
    let summary = set.read(cx).summarize(path);
    let worst = summary.worst?;
    Some((summary, worst))
}

fn format_label(summary: &DiagnosticSummary) -> String {
    let mut parts = Vec::new();
    if summary.error > 0 {
        parts.push(format!("E{}", summary.error));
    }
    if summary.warning > 0 {
        parts.push(format!("W{}", summary.warning));
    }
    if summary.information > 0 {
        parts.push(format!("I{}", summary.information));
    }
    if summary.hint > 0 {
        parts.push(format!("H{}", summary.hint));
    }
    format!(" {} ", parts.join(" "))
}

fn severity_color(severity: DiagnosticSeverity, cx: &App) -> Hsla {
    match severity {
        DiagnosticSeverity::ERROR => cx.theme().diagnostic_error,
        DiagnosticSeverity::WARNING => cx.theme().diagnostic_warning,
        DiagnosticSeverity::INFORMATION => cx.theme().diagnostic_info,
        DiagnosticSeverity::HINT => cx.theme().diagnostic_hint,
        _ => cx.theme().diagnostic_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        diagnostics::DiagnosticSet,
        diff_map::DiffMap,
        display_map::DisplayMap,
        editor::{Editor, EditorMode},
        globals::ExecutorGlobal,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use lsp_types::{Diagnostic, Position, Range};
    use std::{path::PathBuf, sync::Arc};
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_editor(
        cx: &mut TestAppContext,
        path: Option<PathBuf>,
        diag_set: Option<Entity<DiagnosticSet>>,
    ) -> Entity<Editor> {
        cx.update(|cx| {
            let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), ""));
            if let Some(p) = path.clone() {
                buffer.update(cx, |b, cx| b.set_file_path(Some(p), cx));
            }
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
            let editor = cx
                .new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx));
            editor.update(cx, |ed, cx| {
                ed.set_file_path(path, cx);
                if let Some(set) = diag_set {
                    ed.set_diagnostic_set(Some(set), cx);
                }
            });
            editor
        })
    }

    fn new_diag_set(cx: &mut TestAppContext) -> Entity<DiagnosticSet> {
        cx.update(|cx| cx.new(|_| DiagnosticSet::new()))
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

    fn new_badge(cx: &mut TestAppContext) -> Entity<DiagnosticsBadge> {
        cx.update(|cx| cx.new(|_| DiagnosticsBadge::new()))
    }

    #[test]
    fn new_starts_empty() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let badge = new_badge(&mut cx);
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn binds_to_editor_with_diagnostics_reports_summary() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let path = PathBuf::from("/tmp/repo/a.rs");
        let diag_set = new_diag_set(&mut cx);
        diag_set.update(&mut cx, |s, cx| {
            s.replace_for_path(
                path.clone(),
                vec![
                    diag(DiagnosticSeverity::ERROR, "e1"),
                    diag(DiagnosticSeverity::WARNING, "w1"),
                ],
                cx,
            )
        });
        let editor = new_editor(&mut cx, Some(path), Some(diag_set));
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));

        badge.read_with(&cx, |b, _| {
            let (summary, worst) = b.summary().expect("summary");
            assert_eq!(summary.error, 1);
            assert_eq!(summary.warning, 1);
            assert_eq!(summary.information, 0);
            assert_eq!(summary.hint, 0);
            assert_eq!(*worst, DiagnosticSeverity::ERROR);
        });
    }

    #[test]
    fn editor_without_diagnostic_set_yields_no_summary() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let editor = new_editor(&mut cx, Some(PathBuf::from("/tmp/repo/a.rs")), None);
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn editor_without_path_yields_no_summary() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let diag_set = new_diag_set(&mut cx);
        let editor = new_editor(&mut cx, None, Some(diag_set));
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn clear_drops_summary_when_active_item_is_none() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let path = PathBuf::from("/tmp/repo/a.rs");
        let diag_set = new_diag_set(&mut cx);
        diag_set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "e")], cx)
        });
        let editor = new_editor(&mut cx, Some(path), Some(diag_set));
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(None, cx));
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn rebinding_swaps_summary_source() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let path_a = PathBuf::from("/tmp/repo/a.rs");
        let path_b = PathBuf::from("/tmp/repo/b.rs");
        let diag_set = new_diag_set(&mut cx);
        diag_set.update(&mut cx, |s, cx| {
            s.replace_for_path(
                path_a.clone(),
                vec![diag(DiagnosticSeverity::ERROR, "ea")],
                cx,
            );
            s.replace_for_path(
                path_b.clone(),
                vec![diag(DiagnosticSeverity::WARNING, "wb")],
                cx,
            );
        });
        let editor_a = new_editor(&mut cx, Some(path_a), Some(diag_set.clone()));
        let editor_b = new_editor(&mut cx, Some(path_b), Some(diag_set));
        let badge = new_badge(&mut cx);
        let handle_a: Box<dyn ItemHandle> = Box::new(editor_a);
        let handle_b: Box<dyn ItemHandle> = Box::new(editor_b);
        badge.update(&mut cx, |b, cx| {
            b.set_active_pane_item(Some(&*handle_a), cx)
        });
        badge.update(&mut cx, |b, cx| {
            b.set_active_pane_item(Some(&*handle_b), cx)
        });

        badge.read_with(&cx, |b, _| {
            let (summary, worst) = b.summary().expect("summary");
            assert_eq!(summary.error, 0);
            assert_eq!(summary.warning, 1);
            assert_eq!(*worst, DiagnosticSeverity::WARNING);
        });
    }

    #[test]
    fn diagnostic_publish_propagates_through_editor_event() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let path = PathBuf::from("/tmp/repo/a.rs");
        let diag_set = new_diag_set(&mut cx);
        let editor = new_editor(&mut cx, Some(path.clone()), Some(diag_set.clone()));
        let badge = new_badge(&mut cx);
        let handle: Box<dyn ItemHandle> = Box::new(editor);
        badge.update(&mut cx, |b, cx| b.set_active_pane_item(Some(&*handle), cx));
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));

        diag_set.update(&mut cx, |s, cx| {
            s.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::HINT, "h")], cx)
        });
        cx.run_until_parked();
        badge.read_with(&cx, |b, _| {
            let (summary, worst) = b.summary().expect("summary");
            assert_eq!(summary.hint, 1);
            assert_eq!(*worst, DiagnosticSeverity::HINT);
        });

        diag_set.update(&mut cx, |s, cx| s.clear(path.clone(), cx));
        cx.run_until_parked();
        badge.read_with(&cx, |b, _| assert!(b.summary().is_none()));
    }

    #[test]
    fn format_label_omits_zero_severities() {
        let summary = DiagnosticSummary {
            error: 2,
            hint: 4,
            ..Default::default()
        };
        assert_eq!(format_label(&summary), " E2 H4 ");
    }

    #[test]
    fn format_label_renders_all_severities() {
        let summary = DiagnosticSummary {
            error: 1,
            warning: 2,
            information: 3,
            hint: 4,
            worst: Some(DiagnosticSeverity::ERROR),
        };
        assert_eq!(format_label(&summary), " E1 W2 I3 H4 ");
    }
}
