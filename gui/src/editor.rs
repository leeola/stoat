pub mod mouse;

use crate::{
    diff_map::{DiffMap, DiffMapEvent},
    display_map::{DisplayMap, DisplayMapEvent},
    multi_buffer::{MultiBuffer, MultiBufferEvent},
};
use gpui::{Context, Entity, EventEmitter, Subscription};
use stoat::{jumplist::JumpList, selection::SelectionsCollection};
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

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

    pub fn selections_mut(&mut self) -> &mut SelectionsCollection {
        &mut self.selections
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

    /// Insert `text` at every selection in this editor. Range
    /// selections are replaced by `text`; empty selections (cursors)
    /// have `text` inserted at their position. After all edits each
    /// selection collapses to a single cursor immediately after the
    /// inserted text in the post-edit buffer.
    ///
    /// Edits are applied in reverse-offset order so an earlier
    /// edit's range is still valid after later edits have committed.
    /// Each cursor's post-edit offset accounts for cumulative shifts
    /// from edits at earlier offsets: for cursor `i` (ascending
    /// offset order) the new offset is
    /// `pre_start_i + text.len() + sum_{j<i}(text.len() - (pre_end_j - pre_start_j))`.
    /// Multi-excerpt buffers are skipped with a `tracing::warn` --
    /// the multi-buffer edit surface is not yet built.
    pub fn apply_text_to_all_cursors(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "apply_text_to_all_cursors on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let mut ascending: Vec<(usize, std::ops::Range<usize>)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .map(|sel| {
                    let start = snapshot.resolve_anchor(&sel.start);
                    let end = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    (sel.id, lo..hi)
                })
                .collect()
        };
        ascending.sort_by_key(|(_, range)| range.start);

        let text_len = text.len();
        let mut cumulative_shift: isize = 0;
        let mut post_offsets: Vec<(usize, usize)> = Vec::with_capacity(ascending.len());
        for (id, range) in &ascending {
            let post = (range.start as isize + cumulative_shift) as usize + text_len;
            post_offsets.push((*id, post));
            cumulative_shift += text_len as isize - (range.end - range.start) as isize;
        }

        for (_id, range) in ascending.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(range.clone(), text, cx));
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_disjoint: Vec<Selection<Anchor>> = post_offsets
            .into_iter()
            .map(|(id, post)| {
                let anchor = new_snapshot.anchor_at(post, Bias::Left);
                Selection {
                    id,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        new_disjoint.sort_by_key(|s| s.id);

        self.selections.replace_with(new_disjoint, &new_snapshot);
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

    fn cursor_offsets(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Vec<usize> {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| snapshot.resolve_anchor(&s.start))
                .collect()
        })
    }

    fn seed_cursors(editor: &Entity<Editor>, cx: &mut TestAppContext, offsets: &[usize]) {
        let offsets = offsets.to_vec();
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let cursors: Vec<Selection<Anchor>> = offsets
                .iter()
                .enumerate()
                .map(|(idx, off)| {
                    let anchor = snapshot.anchor_at(*off, Bias::Left);
                    Selection {
                        id: 100 + idx,
                        start: anchor,
                        end: anchor,
                        reversed: false,
                        goal: SelectionGoal::None,
                    }
                })
                .collect();
            ed.selections_mut().replace_with(cursors, &snapshot);
        });
    }

    #[test]
    fn apply_text_to_all_cursors_inserts_at_default_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("X", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "Xhello");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn apply_text_to_all_cursors_replaces_range_selection() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.anchor_at(0, Bias::Left);
            let end = snapshot.anchor_at(3, Bias::Left);
            let sel = Selection {
                id: 200,
                start,
                end,
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("Y", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "Ylo");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn apply_text_to_all_cursors_inserts_at_each_of_multiple_cursors() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        seed_cursors(&editor, &mut cx, &[1, 3]);

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("X", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hXelXlo");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![2, 5]);
    }

    #[test]
    fn apply_text_to_all_cursors_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("Z", cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(!observed.is_empty(), "expected at least one Changed event");
    }
}
