//! Wakers for the background tasks the run loop polls.
//!
//! The run loop polls each background task once per pass, and a task asks
//! for that pass before its result is readable. A pool closure notifies before
//! its value crosses to the task the loop polls, and
//! [`Executor::spawn_with_redraw`](stoat_scheduler::Executor::spawn_with_redraw)
//! notifies before its task stores the value. So the pass the notify starts
//! sometimes polls a task that is still pending. A noop waker then drops the
//! task's completion, and the result waits for an unrelated event.

use std::{
    sync::Arc,
    task::{Wake, Waker},
};
use tokio::sync::Notify;

/// A waker that notifies `notify` when the task it polls completes.
///
/// Poll a background task with the waker of the notify that the task wakes.
/// A task that is still pending at the poll then asks for another pass when
/// its result lands.
pub(crate) fn notify_waker(notify: &Arc<Notify>) -> Waker {
    Waker::from(Arc::new(NotifyWaker(notify.clone())))
}

struct NotifyWaker(Arc<Notify>);

impl Wake for NotifyWaker {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
}
