use crate::buffer::{Buffer, BufferEvent};
use gpui::{Context, Entity, EntityId, EventEmitter, Subscription};
use std::{collections::HashMap, ops::Range};
use stoat::multi_buffer::{ExcerptId, MultiBuffer as InnerMultiBuffer, MultiBufferSnapshot};

/// Entity-shaped wrapper around [`stoat::multi_buffer::MultiBuffer`].
/// Subscribes to each child [`Entity<Buffer>`] once even when the same
/// buffer backs several excerpts, so a child edit re-emits exactly one
/// [`MultiBufferEvent`].
pub struct MultiBuffer {
    inner: InnerMultiBuffer,
    bindings: HashMap<EntityId, BufferBinding>,
    excerpt_to_entity: HashMap<ExcerptId, EntityId>,
}

struct BufferBinding {
    entity: Entity<Buffer>,
    _subscription: Subscription,
    excerpt_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiBufferEvent {
    Edited,
    Saved,
    Reloaded,
    DiagnosticsUpdated,
    LanguageChanged,
    ExcerptsAdded(Vec<ExcerptId>),
    ExcerptsRemoved(Vec<ExcerptId>),
}

impl EventEmitter<MultiBufferEvent> for MultiBuffer {}

impl MultiBuffer {
    pub fn singleton(buffer: Entity<Buffer>, cx: &mut Context<'_, Self>) -> Self {
        let buffer_id = buffer.read(cx).read(|b| b.buffer_id());
        let shared = buffer.read(cx).shared().clone();
        let entity_id = buffer.entity_id();
        let subscription = subscribe_buffer(&buffer, cx);
        let mut bindings = HashMap::new();
        bindings.insert(
            entity_id,
            BufferBinding {
                entity: buffer,
                _subscription: subscription,
                excerpt_count: 1,
            },
        );
        Self {
            inner: InnerMultiBuffer::singleton(buffer_id, shared),
            bindings,
            excerpt_to_entity: HashMap::new(),
        }
    }

    pub fn is_singleton(&self) -> bool {
        self.inner.is_singleton()
    }

    pub fn as_singleton(&self) -> Option<&Entity<Buffer>> {
        if !self.is_singleton() {
            return None;
        }
        self.bindings.values().next().map(|b| &b.entity)
    }

    pub fn snapshot(&self) -> MultiBufferSnapshot {
        self.inner.snapshot()
    }

    pub fn buffer_count(&self) -> usize {
        self.bindings.len()
    }

    pub fn insert_excerpts(
        &mut self,
        buffer: Entity<Buffer>,
        ranges: Vec<Range<usize>>,
        cx: &mut Context<'_, Self>,
    ) -> Vec<ExcerptId> {
        let buffer_id = buffer.read(cx).read(|b| b.buffer_id());
        let shared = buffer.read(cx).shared().clone();
        let entity_id = buffer.entity_id();
        let added = self.inner.insert_excerpts(buffer_id, shared, ranges);

        let binding = self
            .bindings
            .entry(entity_id)
            .or_insert_with(|| BufferBinding {
                _subscription: subscribe_buffer(&buffer, cx),
                entity: buffer,
                excerpt_count: 0,
            });
        binding.excerpt_count += added.len();
        for id in &added {
            self.excerpt_to_entity.insert(*id, entity_id);
        }

        cx.emit(MultiBufferEvent::ExcerptsAdded(added.clone()));
        cx.notify();
        added
    }

    pub fn remove_excerpts(&mut self, ids: &[ExcerptId], cx: &mut Context<'_, Self>) {
        if ids.is_empty() {
            return;
        }
        self.inner.remove_excerpts(ids);
        for id in ids {
            let Some(entity_id) = self.excerpt_to_entity.remove(id) else {
                continue;
            };
            if let Some(binding) = self.bindings.get_mut(&entity_id) {
                binding.excerpt_count = binding.excerpt_count.saturating_sub(1);
                if binding.excerpt_count == 0 {
                    self.bindings.remove(&entity_id);
                }
            }
        }
        cx.emit(MultiBufferEvent::ExcerptsRemoved(ids.to_vec()));
        cx.notify();
    }
}

fn subscribe_buffer(buffer: &Entity<Buffer>, cx: &mut Context<'_, MultiBuffer>) -> Subscription {
    cx.subscribe(buffer, |_, _, event: &BufferEvent, cx| {
        let re_emit = match event {
            BufferEvent::Edited => MultiBufferEvent::Edited,
            BufferEvent::Saved => MultiBufferEvent::Saved,
            BufferEvent::Reloaded => MultiBufferEvent::Reloaded,
            BufferEvent::DiagnosticsUpdated => MultiBufferEvent::DiagnosticsUpdated,
            BufferEvent::LanguageChanged => MultiBufferEvent::LanguageChanged,
        };
        cx.emit(re_emit);
        cx.notify();
    })
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::buffer::BufferId;

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            multi: &Entity<MultiBuffer>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<MultiBufferEvent>>>) {
            let events: Arc<Mutex<Vec<MultiBufferEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let multi = multi.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&multi, move |_, _, event: &MultiBufferEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<MultiBufferEvent>>>) -> Vec<MultiBufferEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn new_buffer(cx: &mut TestAppContext, id: u64, text: &str) -> Entity<Buffer> {
        cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(id), text)))
    }

    fn new_singleton(cx: &mut TestAppContext, buffer: Entity<Buffer>) -> Entity<MultiBuffer> {
        cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
    }

    #[test]
    fn singleton_exposes_underlying_buffer() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, 0, "hello");
        let multi = new_singleton(&mut cx, buffer.clone());

        let exposed_id = multi
            .read_with(&cx, |m, _| m.as_singleton().map(|b| b.entity_id()))
            .expect("singleton returns the underlying buffer");
        assert_eq!(exposed_id, buffer.entity_id());
        assert!(multi.read_with(&cx, |m, _| m.is_singleton()));
    }

    #[test]
    fn child_edit_re_emits_edited() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, 0, "abc");
        let multi = new_singleton(&mut cx, buffer.clone());
        let (_recorder, events) = Recorder::install(&mut cx, &multi);

        buffer.update(&mut cx, |b, cx| b.edit(3..3, "d", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![MultiBufferEvent::Edited]);
    }

    #[test]
    fn child_save_re_emits_saved() {
        let mut cx = TestAppContext::single();
        let buffer = new_buffer(&mut cx, 0, "x");
        buffer.update(&mut cx, |b, cx| b.edit(1..1, "y", cx));
        let multi = new_singleton(&mut cx, buffer.clone());
        let (_recorder, events) = Recorder::install(&mut cx, &multi);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![MultiBufferEvent::Saved]);
    }

    #[test]
    fn insert_excerpts_emits_excerpts_added_and_grows_buffer_count() {
        let mut cx = TestAppContext::single();
        let buffer_a = new_buffer(&mut cx, 0, "alpha");
        let buffer_b = new_buffer(&mut cx, 1, "bravo");
        let multi = new_singleton(&mut cx, buffer_a);
        let (_recorder, events) = Recorder::install(&mut cx, &multi);

        let added = multi.update(&mut cx, |m, cx| m.insert_excerpts(buffer_b, vec![0..5], cx));
        cx.run_until_parked();

        assert_eq!(added.len(), 1);
        assert_eq!(drain(&events), vec![MultiBufferEvent::ExcerptsAdded(added)]);
        assert_eq!(multi.read_with(&cx, |m, _| m.buffer_count()), 2);
        assert!(!multi.read_with(&cx, |m, _| m.is_singleton()));
    }

    #[test]
    fn remove_excerpts_emits_excerpts_removed_and_drops_binding() {
        let mut cx = TestAppContext::single();
        let buffer_a = new_buffer(&mut cx, 0, "alpha");
        let buffer_b = new_buffer(&mut cx, 1, "bravo");
        let multi = new_singleton(&mut cx, buffer_a);
        let added = multi.update(&mut cx, |m, cx| m.insert_excerpts(buffer_b, vec![0..5], cx));
        let (_recorder, events) = Recorder::install(&mut cx, &multi);

        multi.update(&mut cx, |m, cx| m.remove_excerpts(&added, cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![MultiBufferEvent::ExcerptsRemoved(added)]
        );
        assert_eq!(multi.read_with(&cx, |m, _| m.buffer_count()), 1);
    }

    #[test]
    fn duplicate_buffer_subscribes_once() {
        let mut cx = TestAppContext::single();
        let buffer_a = new_buffer(&mut cx, 0, "alpha");
        let buffer_b = new_buffer(&mut cx, 1, "hello world");
        let multi = new_singleton(&mut cx, buffer_a);
        multi.update(&mut cx, |m, cx| {
            m.insert_excerpts(buffer_b.clone(), vec![0..5, 6..11], cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &multi);

        buffer_b.update(&mut cx, |b, cx| b.edit(11..11, "!", cx));
        cx.run_until_parked();

        assert_eq!(
            drain(&events),
            vec![MultiBufferEvent::Edited],
            "duplicate buffer must subscribe exactly once"
        );
        assert_eq!(multi.read_with(&cx, |m, _| m.buffer_count()), 2);
    }
}
