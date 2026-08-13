mod clock;
mod executor;
mod local_scheduler;
#[cfg(any(test, feature = "test-support"))]
mod test_scheduler;
#[cfg(test)]
mod tests;
mod tokio_scheduler;

#[cfg(any(test, feature = "test-support"))]
pub use clock::TestClock;
pub use clock::{Clock, LocalClock};
pub use executor::{Executor, Task};
use futures::channel::oneshot;
pub use local_scheduler::LocalScheduler;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
#[cfg(any(test, feature = "test-support"))]
pub use test_scheduler::TestScheduler;
pub use tokio_scheduler::TokioScheduler;

pub type Runnable = async_task::Runnable;

pub trait Scheduler: Send + Sync {
    fn schedule(&self, runnable: Runnable);
    fn timer(&self, duration: Duration) -> Timer;
    fn clock(&self) -> &dyn Clock;

    /// Submit a synchronous closure to a worker that does not block the
    /// scheduler's own progress. Production schedulers route this to a
    /// dedicated blocking pool so the runtime stays interactive while
    /// `work` runs (e.g. directory walks, large file reads). Test
    /// schedulers run `work` inline since they have no real concurrency.
    fn schedule_blocking(&self, work: Box<dyn FnOnce() + Send + 'static>);
}

/// A future that resolves once its scheduler decides the requested duration
/// has passed.
///
/// Dropping one before it resolves cancels the wait outright, so a debounce
/// that re-arms on every keystroke leaves nothing behind.
pub struct Timer(TimerInner);

enum TimerInner {
    /// Resolves when the scheduler drops the paired sender, which is how a
    /// scheduler running its own clock expires a timer on demand.
    Signalled(oneshot::Receiver<()>),
    /// Resolves when the scheduler's own wait completes. Dropping it drops
    /// that wait, which is what makes cancellation free.
    Waiting(Pin<Box<dyn Future<Output = ()> + Send>>),
}

impl Timer {
    /// A timer the scheduler fires by dropping or sending on the paired
    /// sender.
    pub(crate) fn signalled(receiver: oneshot::Receiver<()>) -> Self {
        Self(TimerInner::Signalled(receiver))
    }

    /// A timer that resolves when `wait` does.
    ///
    /// `wait` is first polled when the timer is, not when it is built, so a
    /// scheduler may hand over work that needs its runtime in scope.
    pub(crate) fn waiting(wait: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(TimerInner::Waiting(Box::pin(wait)))
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.0 {
            TimerInner::Signalled(receiver) => match Pin::new(receiver).poll(cx) {
                Poll::Ready(_) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            },
            TimerInner::Waiting(wait) => wait.as_mut().poll(cx),
        }
    }
}
