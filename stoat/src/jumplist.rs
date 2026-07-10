//! A pane-owned, cross-buffer history of the positions the user marked or
//! jumped away from, walked with Helix's semantics.
//!
//! Each [`JumpEntry`] pairs a buffer with the selection set that was live
//! there, held as anchors so the positions ride the fragment tree through
//! edits and resolve lazily at jump time. The list lives on the pane rather
//! than an editor so it survives the `EditorState` swap every cross-file open
//! performs.
//!
//! The walk is ported from Helix's `JumpList`
//! (`references/helix/helix-view/src/view.rs`) over the shared [`NavList`]
//! cursor primitive. Recording truncates forward history and dedups against
//! the back entry, a 30-entry cap drops from the front, a backward step at the
//! tip records the live position first, and a self-skip avoids re-selecting
//! the current spot.

use crate::{buffer_registry::BufferRegistry, nav_list::NavList};
use stoat_text::{Anchor, BufferId, Selection};

/// Retained-entry cap. Once exceeded, the oldest entries drop from the front,
/// matching Helix's jumplist.
const JUMP_LIST_CAPACITY: usize = 30;

/// One recorded position, a buffer plus the selection set that was live there,
/// held as anchors so it tracks edits until resolved at jump time.
#[derive(Debug, Clone)]
pub(crate) struct JumpEntry {
    pub(crate) buffer_id: BufferId,
    pub(crate) selections: Vec<Selection<Anchor>>,
}

/// Cross-buffer jump history over the shared [`NavList`] cursor primitive.
#[derive(Debug, Clone, Default)]
pub(crate) struct JumpList {
    list: NavList<JumpEntry>,
}

impl JumpList {
    pub(crate) fn entries(&self) -> &[JumpEntry] {
        self.list.entries()
    }

    pub(crate) fn cursor(&self) -> usize {
        self.list.cursor()
    }

    /// Position the walk cursor at `cursor` (clamped to the tip), so the next
    /// [`Self::backward`] walks from there. Used by the picker to resume the
    /// walk from a chosen entry.
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.list.set_cursor(cursor);
    }

    /// Record `entry` as the newest position, dropping forward history and
    /// skipping a push that duplicates the current back entry by resolved
    /// shape. Returns how many oldest entries were dropped to stay within
    /// capacity, so a caller walking backward can adjust its target index.
    pub(crate) fn push(&mut self, entry: JumpEntry, buffers: &BufferRegistry) -> usize {
        self.list.truncate_forward();
        if self
            .list
            .back()
            .is_some_and(|back| same_shape(back, &entry, buffers))
        {
            return 0;
        }
        let mut removed = 0;
        while self.list.len() >= JUMP_LIST_CAPACITY {
            self.list.pop_front();
            removed += 1;
        }
        self.list.push_tip(entry);
        removed
    }

    /// Walk `count` entries toward newer positions and return the entry to jump
    /// to. All-or-nothing: `None` without moving when `count` would step past
    /// the newest entry.
    pub(crate) fn forward(&mut self, count: usize) -> Option<&JumpEntry> {
        self.list.step_stop(count as isize)
    }

    /// Walk `count` entries toward older positions and return the entry to jump
    /// to.
    ///
    /// At the tip the `live` position is recorded first, so a later
    /// [`Self::forward`] can return to it. The landing entry is skipped when it
    /// resolves to `live`'s shape, so a backward step from a just-recorded spot
    /// lands on the previous entry rather than re-selecting the current one.
    /// `None` without moving when `count` would step past the oldest entry.
    pub(crate) fn backward(
        &mut self,
        live: JumpEntry,
        buffers: &BufferRegistry,
        count: usize,
    ) -> Option<&JumpEntry> {
        let cursor = self.list.cursor();
        if cursor < count {
            return None;
        }
        let mut target = cursor - count;
        if cursor == self.list.len() {
            let removed = self.push(live.clone(), buffers);
            target = target.saturating_sub(removed);
        }
        self.list.set_cursor(target);
        if self
            .list
            .entries()
            .get(target)
            .is_some_and(|entry| same_shape(entry, &live, buffers))
        {
            target = target.checked_sub(1)?;
            self.list.set_cursor(target);
        }
        self.list.entries().get(target)
    }

    /// Drop every entry pointing into `buffer_id`, e.g. when its buffer closes.
    pub(crate) fn remove_buffer(&mut self, buffer_id: BufferId) {
        self.list.retain(|entry| entry.buffer_id != buffer_id);
    }
}

/// Whether two entries select the same buffer positions, comparing resolved
/// offsets rather than anchors. Anchor identity carries a creation timestamp
/// that defeats value equality, so two anchors at the same offset are unequal
/// as values. A buffer that cannot be resolved compares unequal, so an
/// unresolvable entry is never deduped away or skipped over.
fn same_shape(a: &JumpEntry, b: &JumpEntry, buffers: &BufferRegistry) -> bool {
    if a.buffer_id != b.buffer_id {
        return false;
    }
    match (resolved_shape(a, buffers), resolved_shape(b, buffers)) {
        (Some(sa), Some(sb)) => sa == sb,
        _ => false,
    }
}

/// Resolve an entry's selections to `(start, end, reversed)` offset tuples
/// against its buffer, or `None` when the buffer is gone.
fn resolved_shape(
    entry: &JumpEntry,
    buffers: &BufferRegistry,
) -> Option<Vec<(usize, usize, bool)>> {
    let buffer = buffers.get(entry.buffer_id)?;
    let guard = buffer.read().ok()?;
    Some(
        entry
            .selections
            .iter()
            .map(|s| {
                (
                    guard.resolve_anchor(&s.start),
                    guard.resolve_anchor(&s.end),
                    s.reversed,
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_registry::BufferRegistry;
    use std::path::Path;
    use stoat_text::{Bias, SelectionGoal};

    /// A single-cursor entry at `offset` in `buffer_id`, resolved against
    /// `buffers` so the anchor tracks that buffer.
    fn entry(buffers: &BufferRegistry, buffer_id: BufferId, offset: usize) -> JumpEntry {
        let buffer = buffers.get(buffer_id).expect("buffer open");
        let guard = buffer.read().expect("buffer readable");
        let anchor = guard.anchor_at(offset, Bias::Right);
        JumpEntry {
            buffer_id,
            selections: vec![Selection {
                id: 0,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }],
        }
    }

    fn one_buffer() -> (BufferRegistry, BufferId) {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/a.rs"), "0123456789abcdefghij");
        (buffers, id)
    }

    fn offsets(list: &JumpList, buffers: &BufferRegistry) -> Vec<usize> {
        list.entries()
            .iter()
            .map(|e| resolved_shape(e, buffers).unwrap()[0].0)
            .collect()
    }

    #[test]
    fn push_records_and_truncates_forward_history() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, id, 1), &buffers);
        jl.push(entry(&buffers, id, 2), &buffers);
        jl.push(entry(&buffers, id, 3), &buffers);
        // Walk back two, then a fresh push drops the forward tail.
        jl.backward(entry(&buffers, id, 9), &buffers, 2);
        jl.push(entry(&buffers, id, 7), &buffers);
        assert_eq!(offsets(&jl, &buffers), vec![1, 7]);
    }

    #[test]
    fn push_dedups_adjacent_same_shape() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, id, 4), &buffers);
        jl.push(entry(&buffers, id, 4), &buffers);
        assert_eq!(offsets(&jl, &buffers), vec![4]);
    }

    #[test]
    fn push_caps_at_capacity_dropping_from_front() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        for i in 0..JUMP_LIST_CAPACITY + 5 {
            jl.push(entry(&buffers, id, i % 20), &buffers);
        }
        assert_eq!(jl.entries().len(), JUMP_LIST_CAPACITY);
    }

    #[test]
    fn backward_at_tip_records_live_so_forward_returns() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, id, 2), &buffers);
        jl.push(entry(&buffers, id, 5), &buffers);
        // From live=8 (the tip), backward lands on 5 and records 8.
        let back = jl.backward(entry(&buffers, id, 8), &buffers, 1).unwrap();
        assert_eq!(resolved_shape(back, &buffers).unwrap()[0].0, 5);
        // Forward returns to the recorded live position.
        let fwd = jl.forward(1).unwrap();
        assert_eq!(resolved_shape(fwd, &buffers).unwrap()[0].0, 8);
    }

    #[test]
    fn backward_self_skips_the_just_recorded_position() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, id, 2), &buffers);
        jl.push(entry(&buffers, id, 6), &buffers);
        // live == back (6): backward skips the duplicate and lands on 2.
        let back = jl.backward(entry(&buffers, id, 6), &buffers, 1).unwrap();
        assert_eq!(resolved_shape(back, &buffers).unwrap()[0].0, 2);
    }

    #[test]
    fn forward_is_all_or_nothing() {
        let (buffers, id) = one_buffer();
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, id, 1), &buffers);
        jl.push(entry(&buffers, id, 2), &buffers);
        // Backward at the tip records live=9 and lands on the oldest entry, so
        // the list is [1, 2, 9] with the cursor at 0.
        jl.backward(entry(&buffers, id, 9), &buffers, 2);
        // A forward past the newest entry moves nothing.
        assert!(jl.forward(3).is_none());
        assert_eq!(
            jl.forward(1)
                .map(|e| resolved_shape(e, &buffers).unwrap()[0].0),
            Some(2)
        );
    }

    #[test]
    fn remove_buffer_drops_matching_entries() {
        let mut buffers = BufferRegistry::new();
        let (a, _) = buffers.open(Path::new("/a.rs"), "aaaaaaaa");
        let (b, _) = buffers.open(Path::new("/b.rs"), "bbbbbbbb");
        let mut jl = JumpList::default();
        jl.push(entry(&buffers, a, 1), &buffers);
        jl.push(entry(&buffers, b, 2), &buffers);
        jl.push(entry(&buffers, a, 3), &buffers);
        jl.remove_buffer(a);
        assert_eq!(jl.entries().len(), 1);
        assert_eq!(jl.entries()[0].buffer_id, b);
    }
}
