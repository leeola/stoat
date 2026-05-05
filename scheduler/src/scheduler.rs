mod clock;
mod executor;
mod local_scheduler;
mod test_scheduler;
#[cfg(test)]
mod tests;
mod tokio_scheduler;

pub use clock::{Clock, LocalClock, TestClock};
pub use executor::{Executor, Task};
use futures::channel::oneshot;
pub use local_scheduler::LocalScheduler;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
pub use test_scheduler::TestScheduler;
pub use tokio_scheduler::TokioScheduler;

pub type Runnable = async_task::Runnable;

pub trait Scheduler: Send + Sync {
    fn schedule(&self, runnable: Runnable);
    fn timer(&self, duration: Duration) -> Timer;
    fn clock(&self) -> &dyn Clock;
}

pub struct Timer(oneshot::Receiver<()>);

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}
