use crate::buffer::{Buffer, BufferEvent};
use gpui::{Context, Entity, EventEmitter, Subscription};
use stoat::{
    display_map::BlockProperties, multi_buffer::MultiBuffer as InnerMultiBuffer,
    DisplayMap as InnerDisplayMap, DisplaySnapshot,
};
use stoat_scheduler::Executor;

/// Entity-shaped wrapper around [`stoat::DisplayMap`]. Subscribes to the
/// source [`Entity<Buffer>`] and re-emits [`DisplayMapEvent::Changed`]
/// when the buffer fires `Edited` or `Reloaded`. The inner snapshot
/// cache is version-gated, so consumers obtain a fresh
/// [`DisplaySnapshot`] by calling [`DisplayMap::snapshot`] after a
/// notify; no manual recompute on the wrapper is needed.
pub struct DisplayMap {
    inner: InnerDisplayMap,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayMapEvent {
    Changed,
}

impl EventEmitter<DisplayMapEvent> for DisplayMap {}

impl DisplayMap {
    pub fn new(buffer: Entity<Buffer>, executor: Executor, cx: &mut Context<'_, Self>) -> Self {
        let buffer_id = buffer.read(cx).read(|b| b.buffer_id());
        let shared = buffer.read(cx).shared().clone();
        let multi_buffer = InnerMultiBuffer::singleton(buffer_id, shared);
        let inner = InnerDisplayMap::new(multi_buffer, executor);
        let subscription = cx.subscribe(&buffer, |_, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                cx.emit(DisplayMapEvent::Changed);
                cx.notify();
            }
        });
        Self {
            inner,
            _subscription: subscription,
        }
    }

    /// Return the current display snapshot. Takes `&mut self` because
    /// the inner [`stoat::DisplayMap`] populates its snapshot cache on
    /// the first call after each buffer/diff version bump. Callers
    /// invoke this via `entity.update(cx, |dm, _| dm.snapshot())`.
    pub fn snapshot(&mut self) -> DisplaySnapshot {
        self.inner.snapshot()
    }

    pub(crate) fn insert_blocks(
        &mut self,
        blocks: Vec<BlockProperties>,
        cx: &mut Context<'_, Self>,
    ) {
        if blocks.is_empty() {
            return;
        }
        self.inner.insert_blocks(blocks);
        cx.emit(DisplayMapEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::buffer::BufferId;
    use stoat_scheduler::TestScheduler;

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            display_map: &Entity<DisplayMap>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<DisplayMapEvent>>>) {
            let events: Arc<Mutex<Vec<DisplayMapEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let display_map = display_map.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&display_map, move |_, _, event: &DisplayMapEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn drain(events: &Arc<Mutex<Vec<DisplayMapEvent>>>) -> Vec<DisplayMapEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn new_display_map(
        cx: &mut TestAppContext,
        text: &str,
    ) -> (Entity<Buffer>, Entity<DisplayMap>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = test_executor();
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        (buffer, display_map)
    }

    #[test]
    fn snapshot_reflects_initial_buffer_text() {
        let mut cx = TestAppContext::single();
        let (_buffer, display_map) = new_display_map(&mut cx, "hello world");

        let text = display_map.update(&mut cx, |dm, _| dm.snapshot().text().to_string());
        assert_eq!(text, "hello world");
    }

    #[test]
    fn edit_emits_changed_and_snapshot_reflects_edit() {
        let mut cx = TestAppContext::single();
        let (buffer, display_map) = new_display_map(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &display_map);

        buffer.update(&mut cx, |b, cx| b.edit(2..2, "!", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DisplayMapEvent::Changed]);
        let text = display_map.update(&mut cx, |dm, _| dm.snapshot().text().to_string());
        assert_eq!(text, "hi!");
    }

    #[test]
    fn reload_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, display_map) = new_display_map(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &display_map);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DisplayMapEvent::Changed]);
    }

    #[test]
    fn save_does_not_emit_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, display_map) = new_display_map(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &display_map);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<DisplayMapEvent>::new());
    }

    #[test]
    fn insert_blocks_emits_changed_and_extends_snapshot() {
        use stoat::display_map::{BlockPlacement, BlockProperties, BlockStyle};
        let mut cx = TestAppContext::single();
        let (_buffer, display_map) = new_display_map(&mut cx, "alpha\nbeta");
        let (_recorder, events) = Recorder::install(&mut cx, &display_map);

        display_map.update(&mut cx, |dm, cx| {
            dm.insert_blocks(
                vec![BlockProperties::from_text(
                    BlockPlacement::Above(0),
                    vec!["header".into()],
                    BlockStyle::Fixed,
                )],
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DisplayMapEvent::Changed]);
        let max_row = display_map.update(&mut cx, |dm, _| dm.snapshot().max_point().row);
        assert_eq!(max_row, 2);
    }

    #[test]
    fn insert_blocks_with_empty_input_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (_buffer, display_map) = new_display_map(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &display_map);

        display_map.update(&mut cx, |dm, cx| dm.insert_blocks(Vec::new(), cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<DisplayMapEvent>::new());
    }
}
