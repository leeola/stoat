//! Miscellaneous utilities.
//!
//! `Pushable<T>` is an mpsc-backed push queue: a producer calls `push`
//! at any time; a single consumer awaits items via async iteration.
//! Used for fan-in of wire frames into the host adapter.
//! `unreachable_value!` logs and panics on unexpected match fall-through.

use futures::Stream;
use std::{pin::Pin, task::Context};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Push-to-stream bridge. `push` appends an item; `end` closes the
/// stream; the receiver half is a `Stream<Item = T>` consumers can
/// drive with `StreamExt::next()`.
pub struct Pushable<T> {
    tx: UnboundedSender<T>,
    // Kept on construction; handed out once via `into_stream`.
    rx: Option<UnboundedReceiver<T>>,
}

impl<T: Send + 'static> Default for Pushable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> Pushable<T> {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self { tx, rx: Some(rx) }
    }

    /// Enqueue an item. Returns `Err` when the stream has been dropped.
    pub fn push(&self, item: T) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
        self.tx.send(item)
    }

    /// Close the stream. Subsequent `push` calls fail; the stream will
    /// yield `None` after draining pending items.
    pub fn end(self) {
        drop(self.tx);
        // rx is dropped with self.
    }

    /// Consume and return the underlying receiver as a `Stream`. Can
    /// only be called once; subsequent calls return `None`.
    pub fn into_stream(&mut self) -> Option<impl Stream<Item = T>> {
        self.rx.take().map(UnboundedReceiverStream::new)
    }

    pub fn sender(&self) -> UnboundedSender<T> {
        self.tx.clone()
    }
}

/// Pin projection so the module-internal `Pushable<T>` can be polled
/// directly without extracting the stream first, in test contexts.
impl<T: Send + 'static> Stream for Pushable<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> std::task::Poll<Option<T>> {
        match &mut self.rx {
            Some(rx) => rx.poll_recv(cx),
            None => std::task::Poll::Ready(None),
        }
    }
}

/// Log the unexpected value at warn level and panic. Useful in match
/// arms that should be unreachable but must compile.
#[macro_export]
macro_rules! unreachable_value {
    ($value:expr) => {{
        ::tracing::warn!("unreachable value at {}:{}: {:?}", file!(), line!(), $value);
        panic!("unreachable value: {:?}", $value)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn pushable_yields_items_in_order() {
        let mut pushable: Pushable<u32> = Pushable::new();
        pushable.push(1).unwrap();
        pushable.push(2).unwrap();
        let stream = pushable.into_stream().unwrap();
        tokio::pin!(stream);
        assert_eq!(stream.next().await, Some(1));
        assert_eq!(stream.next().await, Some(2));
    }
}
