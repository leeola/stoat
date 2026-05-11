use crate::{
    diff_map::{DiffMap, DiffMapEvent},
    display_map::{DisplayMap, DisplayMapEvent},
    multi_buffer::{MultiBuffer, MultiBufferEvent},
};
use gpui::{Context, Entity, EventEmitter, Subscription};
use stoat::{jumplist::JumpList, selection::SelectionsCollection};

/// Entity holding the state a single editor view needs:
/// [`Entity<MultiBuffer>`] for the source text, [`Entity<DisplayMap>`]
/// for the visible-line projection, [`Entity<DiffMap>`] for the
/// gutter-strip diff data, the user's selections and jumplist, and
/// the current scroll row.
///
/// Render, mouse handling, action handlers, and `ItemView` registration
/// land in sibling items; this struct exposes only the state fields,
/// a subscription that re-emits child changes as
/// [`EditorEvent::Changed`], and the minimum mutation surface needed to
/// validate the event pipeline.
pub struct Editor {
    multi_buffer: Entity<MultiBuffer>,
    display_map: Entity<DisplayMap>,
    diff_map: Entity<DiffMap>,
    selections: SelectionsCollection,
    scroll_row: u32,
    jumplist: JumpList,
    _subscriptions: [Subscription; 3],
}

/// Single coalesced "editor changed" signal. Subscribers re-render on
/// any event; finer-grained variants are added when a consumer needs
/// to discriminate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    Changed,
}

impl EventEmitter<EditorEvent> for Editor {}

impl Editor {
    pub fn new(
        multi_buffer: Entity<MultiBuffer>,
        display_map: Entity<DisplayMap>,
        diff_map: Entity<DiffMap>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mb_sub = cx.subscribe(&multi_buffer, |_, _, _event: &MultiBufferEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        let dm_sub = cx.subscribe(&display_map, |_, _, _event: &DisplayMapEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        let diff_sub = cx.subscribe(&diff_map, |_, _, _event: &DiffMapEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        Self {
            multi_buffer,
            display_map,
            diff_map,
            selections: SelectionsCollection::new(),
            scroll_row: 0,
            jumplist: JumpList::new(),
            _subscriptions: [mb_sub, dm_sub, diff_sub],
        }
    }

    pub fn multi_buffer(&self) -> &Entity<MultiBuffer> {
        &self.multi_buffer
    }

    pub fn display_map(&self) -> &Entity<DisplayMap> {
        &self.display_map
    }

    pub fn diff_map(&self) -> &Entity<DiffMap> {
        &self.diff_map
    }

    pub fn selections(&self) -> &SelectionsCollection {
        &self.selections
    }

    pub fn scroll_row(&self) -> u32 {
        self.scroll_row
    }

    pub fn jumplist(&self) -> &JumpList {
        &self.jumplist
    }

    pub fn set_scroll_row(&mut self, row: u32, cx: &mut Context<'_, Self>) {
        if self.scroll_row == row {
            return;
        }
        self.scroll_row = row;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use gpui::{AppContext, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            editor: &Entity<Editor>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<EditorEvent>>>) {
            let events: Arc<Mutex<Vec<EditorEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let editor = editor.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&editor, move |_, _, event: &EditorEvent, _| {
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

    fn drain(events: &Arc<Mutex<Vec<EditorEvent>>>) -> Vec<EditorEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn new_editor(cx: &mut TestAppContext, text: &str) -> (Entity<Buffer>, Entity<Editor>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = test_executor();
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        let editor =
            cx.update(|cx| cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, cx)));
        (buffer, editor)
    }

    #[test]
    fn new_initializes_default_state() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.scroll_row(), 0);
            assert_eq!(ed.selections().all_anchors().len(), 1);
            assert_eq!(ed.jumplist().entries(), &[] as &[usize]);
            assert_eq!(ed.jumplist().cursor(), 0);
        });
    }

    #[test]
    fn buffer_edit_re_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(
            !observed.is_empty(),
            "expected at least one Changed event from buffer edit",
        );
    }

    #[test]
    fn set_scroll_row_updates_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_scroll_row(7, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![EditorEvent::Changed]);
        editor.read_with(&cx, |ed, _| assert_eq!(ed.scroll_row(), 7));
    }

    #[test]
    fn set_scroll_row_same_value_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_scroll_row(0, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    #[test]
    fn accessors_return_stored_entities() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");

        let (mb_id, dm_id, diff_id) = editor.read_with(&cx, |ed, _| {
            (
                ed.multi_buffer().entity_id(),
                ed.display_map().entity_id(),
                ed.diff_map().entity_id(),
            )
        });
        assert_ne!(mb_id, dm_id);
        assert_ne!(mb_id, diff_id);
        assert_ne!(dm_id, diff_id);
    }
}
