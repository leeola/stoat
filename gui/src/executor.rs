//! Bridge between gpui's main thread and stoat's async runtime.
//!
//! The GUI window runs on gpui's main thread. Async work spawned by
//! gui entities goes through [`stoat_scheduler::Executor`] (the
//! canonical async runtime; see `scheduler/src/executor.rs` module
//! docs). The production binary backs that executor with
//! [`stoat_scheduler::TokioScheduler`] so Tokio-bound hosts (LSP,
//! Claude Code, fs watcher) share the same runtime. Completion hops
//! back to gpui's foreground via [`spawn_with_entity`].

use gpui::{AsyncApp, Context, Task, WeakEntity};
use std::future::Future;
use stoat_scheduler::Executor;

/// Spawn `future` on the Stoat scheduler, then post the result back
/// to the gpui foreground and call `on_complete` against `entity`.
/// Returns a [`gpui::Task`] that resolves on the foreground once the
/// callback has run. The inner work runs on
/// [`stoat_scheduler::Executor`] (the canonical async runtime); only
/// the completion hop touches gpui's foreground.
///
/// If the entity has been released by the time the future resolves,
/// `on_complete` is skipped silently -- matching `WeakEntity::update`'s
/// own released-entity semantics.
///
/// The `entity` parameter is `WeakEntity<T>` by design: this helper is
/// the sanctioned spawn path for entity-bound async work in the gui
/// crate, and weak handles are what keeps subscribers, executors, and
/// the spawned future itself from forming reference cycles that
/// outlive their owning entity. Wrappers that need to fan out async
/// work go through this helper rather than calling `cx.spawn`
/// directly with a captured strong `Entity<T>` -- the latter pins the
/// entity alive for the duration of the future and is the precise
/// pattern this convention exists to avoid.
pub fn spawn_with_entity<T, F, R>(
    executor: &Executor,
    cx: &AsyncApp,
    entity: WeakEntity<T>,
    future: F,
    on_complete: impl FnOnce(&mut T, R, &mut Context<'_, T>) + 'static,
) -> Task<()>
where
    T: 'static,
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let inner = executor.spawn(future);
    cx.spawn(async move |cx| {
        let result = inner.await;
        let _ = entity.update(cx, move |this, ctx| on_complete(this, result, ctx));
    })
}
