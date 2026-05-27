use crate::buffer::{Buffer, BufferEvent};
use gpui::{Context, Entity, EventEmitter, Subscription};
use stoat::DiffMap as InnerDiffMap;

/// Entity-shaped wrapper around [`stoat::DiffMap`]. Owners of the
/// source git repo push freshly-computed [`stoat::DiffMap`]s through
/// [`DiffMap::set_diff`]; the wrapper emits
/// [`DiffMapEvent::Changed`] so subscribers re-render. Edits on the
/// underlying buffer also emit Changed because the stored diff is
/// stale relative to the new buffer text; subscribers may refetch
/// before relying on it.
pub struct DiffMap {
    inner: InnerDiffMap,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffMapEvent {
    Changed,
}

impl EventEmitter<DiffMapEvent> for DiffMap {}

impl DiffMap {
    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.subscribe(&buffer, |_, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                cx.emit(DiffMapEvent::Changed);
                cx.notify();
            }
        });
        Self {
            inner: InnerDiffMap::default(),
            _subscription: subscription,
        }
    }

    pub fn diff(&self) -> &InnerDiffMap {
        &self.inner
    }

    pub fn set_diff(&mut self, new: InnerDiffMap, cx: &mut Context<'_, Self>) {
        self.inner = new;
        cx.emit(DiffMapEvent::Changed);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.inner.is_empty() && self.inner.base_text().is_none() {
            return;
        }
        self.inner = InnerDiffMap::default();
        cx.emit(DiffMapEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::{
        ops::Range,
        sync::{Arc, Mutex},
    };
    use stoat::{
        buffer::BufferId,
        diff_map::{DiffHunk, DiffHunkStatus},
    };

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            diff_map: &Entity<DiffMap>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<DiffMapEvent>>>) {
            let events: Arc<Mutex<Vec<DiffMapEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let diff_map = diff_map.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&diff_map, move |_, _, event: &DiffMapEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<DiffMapEvent>>>) -> Vec<DiffMapEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn added_hunk(buffer_lines: Range<u32>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Added,
            staged: false,
            buffer_start_line: buffer_lines.start,
            buffer_line_range: buffer_lines,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn new_pair(cx: &mut TestAppContext, text: &str) -> (Entity<Buffer>, Entity<DiffMap>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        (buffer, diff_map)
    }

    #[test]
    fn new_wrapper_reports_empty_diff() {
        let mut cx = TestAppContext::single();
        let (_buffer, diff_map) = new_pair(&mut cx, "hi");
        assert!(diff_map.read_with(&cx, |dm, _| dm.diff().is_empty()));
        assert!(diff_map.read_with(&cx, |dm, _| dm.diff().base_text().is_none()));
    }

    #[test]
    fn set_diff_swaps_inner_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, diff_map) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &diff_map);

        let inner = InnerDiffMap::from_hunks([added_hunk(0..1)], None);
        diff_map.update(&mut cx, |dm, cx| dm.set_diff(inner, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DiffMapEvent::Changed]);
        assert!(!diff_map.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn clear_after_set_emits_changed_and_empties_diff() {
        let mut cx = TestAppContext::single();
        let (_buffer, diff_map) = new_pair(&mut cx, "hi");
        diff_map.update(&mut cx, |dm, cx| {
            dm.set_diff(InnerDiffMap::from_hunks([added_hunk(0..1)], None), cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &diff_map);

        diff_map.update(&mut cx, |dm, cx| dm.clear(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DiffMapEvent::Changed]);
        assert!(diff_map.read_with(&cx, |dm, _| dm.diff().is_empty()));
    }

    #[test]
    fn clear_on_empty_diff_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (_buffer, diff_map) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &diff_map);

        diff_map.update(&mut cx, |dm, cx| dm.clear(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<DiffMapEvent>::new());
    }

    #[test]
    fn buffer_edit_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, diff_map) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &diff_map);

        buffer.update(&mut cx, |b, cx| b.edit(2..2, "!", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![DiffMapEvent::Changed]);
    }

    #[test]
    fn buffer_save_does_not_emit_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, diff_map) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &diff_map);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<DiffMapEvent>::new());
    }
}
