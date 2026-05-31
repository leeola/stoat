//! Ephemeral toast notifications.
//!
//! [`Toast`] is one message; [`ToastView`] is the bottom-right overlay
//! that stacks active toasts and auto-dismisses the transient ones.
//! This is the toast primitive: the workspace stores a [`ToastView`]
//! and exposes show/dismiss, and feature code raises toasts -- both
//! land separately from this module, so its surface is unreferenced
//! outside tests until then.
#![allow(dead_code)]

use crate::{globals::ExecutorGlobal, theme::ActiveTheme, workspace::Workspace};
use gpui::{
    div, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    SharedString, Styled, WeakEntity, Window,
};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const INFO_SUCCESS_TTL: Duration = Duration::from_secs(3);
const WARNING_TTL: Duration = Duration::from_secs(6);

static NEXT_TOAST_ID: AtomicU64 = AtomicU64::new(1);

/// Callback a [`ToastAction`] runs against the workspace when clicked.
type ToastActionFn = Box<dyn Fn(&mut Workspace, &mut Window, &mut Context<'_, Workspace>)>;

/// Process-unique identifier for a live toast, used to dismiss it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToastId(u64);

/// Severity of a toast, selecting its color and default lifetime.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

/// An optional labelled button on a toast that runs `action` against
/// the workspace when clicked.
pub struct ToastAction {
    pub label: SharedString,
    pub action: ToastActionFn,
}

/// A single notification. `ttl` is the auto-dismiss delay; it is
/// [`Duration::ZERO`] for [`ToastKind::Error`], which never
/// auto-dismisses. `created_at` records when the toast was raised.
pub struct Toast {
    pub id: ToastId,
    pub kind: ToastKind,
    pub text: SharedString,
    pub action: Option<ToastAction>,
    pub created_at: Instant,
    pub ttl: Duration,
}

impl Toast {
    pub fn info(text: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Info, text)
    }

    pub fn success(text: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Success, text)
    }

    pub fn warning(text: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Warning, text)
    }

    pub fn error(text: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Error, text)
    }

    /// Attach a labelled action button.
    pub fn with_action(
        mut self,
        label: impl Into<SharedString>,
        action: impl Fn(&mut Workspace, &mut Window, &mut Context<'_, Workspace>) + 'static,
    ) -> Self {
        self.action = Some(ToastAction {
            label: label.into(),
            action: Box::new(action),
        });
        self
    }

    fn new(kind: ToastKind, text: impl Into<SharedString>) -> Self {
        let ttl = match kind {
            ToastKind::Info | ToastKind::Success => INFO_SUCCESS_TTL,
            ToastKind::Warning => WARNING_TTL,
            ToastKind::Error => Duration::ZERO,
        };
        Self {
            id: ToastId(NEXT_TOAST_ID.fetch_add(1, Ordering::Relaxed)),
            kind,
            text: text.into(),
            action: None,
            created_at: Instant::now(),
            ttl,
        }
    }
}

/// Bottom-right overlay that owns the stack of live toasts. Transient
/// toasts auto-dismiss after their `ttl`; [`ToastKind::Error`] toasts
/// stay until dismissed (their `x` button or [`Self::dismiss`]).
pub struct ToastView {
    workspace: Option<WeakEntity<Workspace>>,
    toasts: Vec<Toast>,
}

impl ToastView {
    pub fn new(workspace: Option<WeakEntity<Workspace>>) -> Self {
        Self {
            workspace,
            toasts: Vec::new(),
        }
    }

    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    /// Push `toast` onto the stack. Non-error kinds schedule their own
    /// removal after `ttl` via the Stoat executor; errors persist.
    pub fn push(&mut self, toast: Toast, cx: &mut Context<'_, Self>) {
        let id = toast.id;
        let ttl = toast.ttl;
        let auto_dismiss = toast.kind != ToastKind::Error;
        self.toasts.push(toast);
        cx.notify();

        if !auto_dismiss {
            return;
        }
        let Some(executor) = cx.try_global::<ExecutorGlobal>().map(|g| g.0.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            executor.timer(ttl).await;
            this.update(cx, |view, cx| view.dismiss(id, cx)).ok();
        })
        .detach();
    }

    /// Remove the toast with `id`, if still present.
    pub fn dismiss(&mut self, id: ToastId, cx: &mut Context<'_, Self>) {
        let before = self.toasts.len();
        self.toasts.retain(|toast| toast.id != id);
        if self.toasts.len() != before {
            cx.notify();
        }
    }
}

impl Render for ToastView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let text_color = theme.background;
        let toasts = self.toasts.iter().map(|toast| {
            let bg = match toast.kind {
                ToastKind::Info => theme.diagnostic_info,
                ToastKind::Success => theme.success,
                ToastKind::Warning => theme.diagnostic_warning,
                ToastKind::Error => theme.error,
            };
            let id = toast.id;

            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(bg)
                .text_color(text_color)
                .child(div().child(toast.text.clone()));

            if let Some(action) = &toast.action {
                row = row.child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .child(action.label.clone())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _event, window, cx| {
                                view.run_action(id, window, cx);
                            }),
                        ),
                );
            }

            row.child(div().px_1().child("x").on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _event, _window, cx| view.dismiss(id, cx)),
            ))
        });

        div()
            .absolute()
            .bottom_0()
            .right_0()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .children(toasts)
    }
}

impl ToastView {
    /// Run the action of the toast with `id` against the workspace, then
    /// remove the toast. No-op when the toast or workspace is gone.
    fn run_action(&mut self, id: ToastId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(pos) = self.toasts.iter().position(|toast| toast.id == id) else {
            return;
        };
        let toast = self.toasts.remove(pos);
        cx.notify();
        let Some(action) = toast.action else {
            return;
        };
        let Some(workspace) = self.workspace.clone().and_then(|w| w.upgrade()) else {
            return;
        };
        workspace.update(cx, |workspace, cx| (action.action)(workspace, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, TestAppContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) -> Arc<TestScheduler> {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = Executor::new(scheduler.clone());
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
        scheduler
    }

    fn advance(scheduler: &Arc<TestScheduler>, cx: &mut TestAppContext, by: Duration) {
        cx.executor().advance_clock(by);
        scheduler.advance_clock(by);
        cx.run_until_parked();
    }

    #[test]
    fn constructors_set_kind_and_ttl() {
        assert_eq!(Toast::info("a").kind, ToastKind::Info);
        assert_eq!(Toast::info("a").ttl, INFO_SUCCESS_TTL);
        assert_eq!(Toast::success("a").ttl, INFO_SUCCESS_TTL);
        assert_eq!(Toast::warning("a").ttl, WARNING_TTL);
        assert_eq!(Toast::error("a").kind, ToastKind::Error);
        assert_eq!(Toast::error("a").ttl, Duration::ZERO);
    }

    #[test]
    fn push_then_dismiss_removes_the_toast() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let view: Entity<ToastView> = cx.update(|cx| cx.new(|_| ToastView::new(None)));

        let id = view.update(&mut cx, |view, cx| {
            let toast = Toast::error("boom");
            let id = toast.id;
            view.push(toast, cx);
            id
        });
        view.read_with(&cx, |view, _| assert_eq!(view.toasts().len(), 1));

        view.update(&mut cx, |view, cx| view.dismiss(id, cx));
        view.read_with(&cx, |view, _| assert!(view.toasts().is_empty()));
    }

    #[test]
    fn transient_auto_dismisses_but_error_persists() {
        let mut cx = TestAppContext::single();
        let scheduler = install_executor(&mut cx);
        let view: Entity<ToastView> = cx.update(|cx| cx.new(|_| ToastView::new(None)));

        view.update(&mut cx, |view, cx| {
            view.push(Toast::info("hi"), cx);
            view.push(Toast::error("boom"), cx);
        });
        view.read_with(&cx, |view, _| assert_eq!(view.toasts().len(), 2));

        advance(&scheduler, &mut cx, INFO_SUCCESS_TTL);

        view.read_with(&cx, |view, _| {
            assert_eq!(view.toasts().len(), 1, "the info toast auto-dismissed");
            assert_eq!(
                view.toasts()[0].kind,
                ToastKind::Error,
                "the error toast persists"
            );
        });
    }
}
