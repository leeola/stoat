use crate::{
    item::ItemHandle, lsp_state::LspState, status_bar::StatusItemView, theme::statusbar_text_color,
};
use gpui::{
    div, AnyElement, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window,
};
use stoat::lsp::progress::LspProgressEntry;

/// Status-bar item that surfaces the most recent in-flight LSP
/// progress entry. Renders ` {title}[: {message}][ {pct}%] ` in italic
/// when [`LspState::current`] is `Some`; hides entirely when the
/// state is idle. Subscribes to [`LspState`] notifications via
/// `cx.observe`, mirroring [`crate::status_bar::count_prefix::CountPrefix`]
/// -- LSP progress is workspace-wide, not per-active-editor, so
/// `set_active_pane_item` is a no-op.
pub struct LspProgress {
    lsp_state: Entity<LspState>,
    _subscription: Subscription,
}

impl LspProgress {
    pub fn new(lsp_state: Entity<LspState>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.observe(&lsp_state, |_, _, cx| cx.notify());
        Self {
            lsp_state,
            _subscription: subscription,
        }
    }

    pub fn lsp_state(&self) -> &Entity<LspState> {
        &self.lsp_state
    }
}

impl Render for LspProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let label: Option<AnyElement> = self.lsp_state.read(cx).current().map(|entry| {
            div()
                .px_2()
                .italic()
                .text_color(statusbar_text_color(cx))
                .child(SharedString::from(format_progress_label(entry)))
                .into_any_element()
        });
        div().children(label)
    }
}

impl StatusItemView for LspProgress {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _cx: &mut Context<'_, Self>,
    ) {
    }
}

/// Mirrors `lsp_progress_label` in `stoat/src/render/pane.rs`:
/// joins title, optional message, and optional percentage with the
/// same separators, padding the result with a leading and trailing
/// space so adjacent status items stay visually separated.
fn format_progress_label(entry: &LspProgressEntry) -> String {
    let mut body = entry.title.clone();
    if let Some(message) = &entry.message {
        if !body.is_empty() {
            body.push_str(": ");
        }
        body.push_str(message);
    }
    if let Some(pct) = entry.percentage {
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&format!("{pct}%"));
    }
    if body.is_empty() {
        body.push_str("...");
    }
    format!(" {body} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use lsp_types::{
        NumberOrString, ProgressToken, WorkDoneProgress, WorkDoneProgressBegin,
        WorkDoneProgressEnd, WorkDoneProgressReport,
    };
    use stoat::host::LspNotification;

    fn token() -> ProgressToken {
        NumberOrString::Number(1)
    }

    fn begin(title: &str, message: Option<&str>, percentage: Option<u32>) -> LspNotification {
        LspNotification::Progress {
            token: token(),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: title.to_owned(),
                cancellable: None,
                message: message.map(str::to_owned),
                percentage,
            }),
        }
    }

    fn report(message: Option<&str>, percentage: Option<u32>) -> LspNotification {
        LspNotification::Progress {
            token: token(),
            value: WorkDoneProgress::Report(WorkDoneProgressReport {
                cancellable: None,
                message: message.map(str::to_owned),
                percentage,
            }),
        }
    }

    fn end() -> LspNotification {
        LspNotification::Progress {
            token: token(),
            value: WorkDoneProgress::End(WorkDoneProgressEnd { message: None }),
        }
    }

    fn new_state(cx: &mut TestAppContext) -> Entity<LspState> {
        cx.update(|cx| cx.new(|_| LspState::new()))
    }

    fn new_progress(cx: &mut TestAppContext, state: Entity<LspState>) -> Entity<LspProgress> {
        cx.update(|cx| cx.new(|cx| LspProgress::new(state, cx)))
    }

    #[test]
    fn new_starts_idle_with_no_current_entry() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let item = new_progress(&mut cx, state.clone());
        item.read_with(&cx, |i, cx| {
            assert!(i.lsp_state().read(cx).current().is_none());
        });
    }

    #[test]
    fn progress_begin_makes_current_entry_visible() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let item = new_progress(&mut cx, state.clone());

        state.update(&mut cx, |s, cx| {
            s.update(&begin("indexing", None, Some(10)), cx)
        });
        cx.run_until_parked();

        item.read_with(&cx, |i, cx| {
            let entry = i.lsp_state().read(cx).current().expect("entry");
            assert_eq!(entry.title, "indexing");
            assert_eq!(entry.percentage, Some(10));
        });
    }

    #[test]
    fn progress_end_returns_to_hidden_state() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let item = new_progress(&mut cx, state.clone());
        state.update(&mut cx, |s, cx| {
            s.update(&begin("indexing", None, None), cx)
        });
        cx.run_until_parked();

        state.update(&mut cx, |s, cx| s.update(&end(), cx));
        cx.run_until_parked();

        item.read_with(&cx, |i, cx| {
            assert!(i.lsp_state().read(cx).current().is_none());
        });
    }

    #[test]
    fn set_active_pane_item_is_noop() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let item = new_progress(&mut cx, state.clone());
        cx.update(|cx| {
            item.update(cx, |i, cx| i.set_active_pane_item(None, cx));
        });
        item.read_with(&cx, |i, cx| {
            assert!(i.lsp_state().read(cx).current().is_none());
        });
    }

    #[test]
    fn format_label_title_only() {
        let entry = LspProgressEntry {
            title: "indexing".into(),
            message: None,
            percentage: None,
            sequence: 0,
        };
        assert_eq!(format_progress_label(&entry), " indexing ");
    }

    #[test]
    fn format_label_title_message_and_percentage() {
        let entry = LspProgressEntry {
            title: "indexing".into(),
            message: Some("phase 2".into()),
            percentage: Some(50),
            sequence: 0,
        };
        assert_eq!(format_progress_label(&entry), " indexing: phase 2 50% ");
    }

    #[test]
    fn format_label_empty_title_falls_back_to_ellipsis() {
        let entry = LspProgressEntry {
            title: "".into(),
            message: None,
            percentage: None,
            sequence: 0,
        };
        assert_eq!(format_progress_label(&entry), " ... ");
    }

    #[test]
    fn report_after_begin_reflects_latest_message() {
        let mut cx = TestAppContext::single();
        let state = new_state(&mut cx);
        let item = new_progress(&mut cx, state.clone());
        state.update(&mut cx, |s, cx| {
            s.update(&begin("indexing", None, Some(10)), cx)
        });
        cx.run_until_parked();

        state.update(&mut cx, |s, cx| {
            s.update(&report(Some("phase 2"), Some(50)), cx)
        });
        cx.run_until_parked();

        item.read_with(&cx, |i, cx| {
            let entry = i.lsp_state().read(cx).current().expect("entry");
            assert_eq!(entry.message.as_deref(), Some("phase 2"));
            assert_eq!(entry.percentage, Some(50));
        });
    }
}
