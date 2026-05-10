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
