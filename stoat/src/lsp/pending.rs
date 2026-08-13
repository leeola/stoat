//! One in-flight language-server request per slot.
//!
//! Every LSP feature that answers asynchronously keeps one of these. A trigger
//! arms it behind a debounce, and a pump polls it once per background pass.
//!
//! The slot exists because polling a task from the run loop takes a noop waker,
//! a context, and a pin, and putting it back when it is not ready yet. Written
//! per feature that is six lines of ceremony around one question, repeated for
//! every request kind the editor makes.

use futures::task::noop_waker;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use stoat_scheduler::Task;

/// The slot a feature's in-flight request occupies, empty while none is out.
pub(crate) struct Pending<T>(Option<Task<T>>);

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Pending(None)
    }
}

impl<T> Pending<T> {
    /// Put `task` in the slot, dropping whatever it held.
    ///
    /// Dropping a task cancels it, so a debounced trigger re-arms on every tick
    /// without leaving superseded requests running.
    pub(crate) fn arm(&mut self, task: Task<T>) {
        self.0 = Some(task);
    }

    /// Drop the in-flight request without waiting for its answer.
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    /// Whether a request is out.
    ///
    /// Only tests ask. Production code reads the answer through [`Self::poll`],
    /// which reports the same thing and hands over the value.
    #[cfg(test)]
    pub(crate) fn is_pending(&self) -> bool {
        self.0.is_some()
    }

    /// Take the request's value once it resolves, leaving the slot empty.
    ///
    /// Reports `None` both for an empty slot and for a request still running,
    /// which goes back in the slot. Those are the same answer to the caller:
    /// nothing to apply this pass.
    ///
    /// The value comes back owned rather than borrowed, which releases the
    /// caller's borrow of this slot before the apply step takes the app again.
    pub(crate) fn poll(&mut self) -> Option<T>
    where
        T: Unpin,
    {
        let mut task = self.0.take()?;

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut task).poll(&mut cx) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => {
                self.0 = Some(task);
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_scheduler::TestScheduler;

    #[test]
    fn an_empty_slot_polls_to_nothing() {
        let mut slot: Pending<u8> = Pending::default();

        assert!(!slot.is_pending(), "a fresh slot holds no request");
        assert_eq!(slot.poll(), None, "and polling one answers nothing");
    }

    #[test]
    fn a_resolved_request_yields_its_value_and_empties_the_slot() {
        let scheduler = Arc::new(TestScheduler::new());
        let mut slot = Pending::default();
        slot.arm(scheduler.executor().spawn(async { 7u8 }));
        scheduler.run_until_parked();

        assert!(slot.is_pending(), "the request is out until it is polled");
        assert_eq!(slot.poll(), Some(7), "polling hands the value over");
        assert!(!slot.is_pending(), "and leaves the slot empty");
        assert_eq!(slot.poll(), None, "so a second poll answers nothing");
    }

    #[test]
    fn a_running_request_stays_in_the_slot() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let mut slot = Pending::default();
        let timer = executor.timer(std::time::Duration::from_millis(50));
        slot.arm(executor.spawn(async move {
            timer.await;
            7u8
        }));
        scheduler.run_until_parked();

        assert_eq!(slot.poll(), None, "a request still running answers nothing");
        assert!(slot.is_pending(), "and stays out for the next pass");

        scheduler.advance_clock(std::time::Duration::from_millis(50));
        scheduler.run_until_parked();
        assert_eq!(slot.poll(), Some(7), "the next pass collects it");
    }

    #[test]
    fn clearing_drops_the_request() {
        let scheduler = Arc::new(TestScheduler::new());
        let mut slot = Pending::default();
        slot.arm(scheduler.executor().spawn(async { 7u8 }));
        scheduler.run_until_parked();

        slot.clear();
        assert!(!slot.is_pending(), "clearing empties the slot");
        assert_eq!(slot.poll(), None, "so its answer never arrives");
    }
}
