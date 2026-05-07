use crate::{Scheduler, Timer};
use futures::channel::oneshot;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
pub struct Executor {
    scheduler: Arc<dyn Scheduler>,
}

impl Executor {
    pub fn new(scheduler: Arc<dyn Scheduler>) -> Self {
        Self { scheduler }
    }

    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let scheduler = Arc::clone(&self.scheduler);
        let (runnable, task) = async_task::spawn(future, move |runnable| {
            scheduler.schedule(runnable);
        });
        runnable.schedule();
        Task::Spawned(task)
    }

    /// Run `f` on a worker that does not block the scheduler. Production
    /// schedulers route this to a blocking thread pool so the runtime
    /// stays interactive while `f` executes (e.g. directory walks). The
    /// returned [`Task`] resolves once `f` has produced its value.
    pub fn spawn_blocking<F, R>(&self, f: F) -> Task<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.scheduler.schedule_blocking(Box::new(move || {
            let _ = tx.send(f());
        }));
        self.spawn(async move {
            rx.await
                .expect("spawn_blocking sender dropped without sending")
        })
    }

    pub fn timer(&self, duration: Duration) -> Timer {
        self.scheduler.timer(duration)
    }

    /// Current time according to the scheduler's [`Clock`](crate::Clock).
    /// Tests driven by [`TestScheduler`](crate::TestScheduler) advance this
    /// clock through `advance_clock`; production code reads real wall time.
    pub fn now(&self) -> Instant {
        self.scheduler.clock().now()
    }

    /// Wall-clock time according to the scheduler's [`Clock`](crate::Clock).
    /// Tests driven by [`TestScheduler`](crate::TestScheduler) advance this
    /// in lock-step with [`Executor::now`] through `advance_clock`.
    pub fn system_now(&self) -> SystemTime {
        self.scheduler.clock().system_now()
    }
}

#[must_use]
pub enum Task<T> {
    Ready(Option<T>),
    Spawned(async_task::Task<T>),
}

impl<T> Task<T> {
    pub fn ready(value: T) -> Self {
        Task::Ready(Some(value))
    }

    pub fn detach(self) {
        match self {
            Task::Ready(_) => {},
            Task::Spawned(task) => task.detach(),
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // SAFETY: we never move the inner async_task::Task out of the pin
        match unsafe { self.get_unchecked_mut() } {
            Task::Ready(val) => {
                Poll::Ready(val.take().expect("Task::Ready polled after completion"))
            },
            Task::Spawned(task) => Pin::new(task).poll(cx),
        }
    }
}
