//! A cursor-tracked navigation list, the shared substrate for the jumplist
//! and, later, the callsite trail.
//!
//! A `Vec<E>` of recorded positions plus a `cursor` marking where a walk
//! currently sits. The cursor ranges over `0..=len`: an index in `0..len`
//! points at an entry, and `len` is the *tip* just past the newest entry,
//! where a fresh walk begins and no forward step is possible.
//!
//! [`NavList::truncate_forward`] and [`NavList::push_tip`] build the list,
//! dropping any forward history before appending. [`NavList::step_stop`]
//! walks the cursor, stopping without moving at either end. [`NavList::retain`]
//! filters entries while keeping the cursor over the same surviving position.

/// Generic cursor-tracked list. `E` is the recorded position type (a jump
/// entry, a trail symbol). See the module docs for the cursor model.
#[derive(Debug, Clone)]
pub(crate) struct NavList<E> {
    entries: Vec<E>,
    cursor: usize,
}

impl<E> Default for NavList<E> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
        }
    }
}

impl<E> NavList<E> {
    pub(crate) fn entries(&self) -> &[E] {
        &self.entries
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn back(&self) -> Option<&E> {
        self.entries.last()
    }

    /// Move the cursor to `cursor`, clamped to the valid range `0..=len`.
    /// A value of `len` parks it at the tip.
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.entries.len());
    }

    /// Drop everything ahead of the cursor, discarding forward history so the
    /// next [`Self::push_tip`] appends onto the walked-to position.
    pub(crate) fn truncate_forward(&mut self) {
        self.entries.truncate(self.cursor);
    }

    /// Append `entry` as the newest position and park the cursor at the tip.
    pub(crate) fn push_tip(&mut self, entry: E) {
        self.entries.push(entry);
        self.cursor = self.entries.len();
    }

    /// Remove the oldest entry, shifting the cursor down so it still points at
    /// the same surviving position. For capacity trimming from the front.
    pub(crate) fn pop_front(&mut self) -> Option<E> {
        if self.entries.is_empty() {
            return None;
        }
        self.cursor = self.cursor.saturating_sub(1);
        Some(self.entries.remove(0))
    }

    /// Step the cursor by `delta` and return the entry it lands on. Yields
    /// `None` without moving when the step would pass either end, so a walk
    /// stops at the oldest entry and at the newest entry (never onto the tip).
    pub(crate) fn step_stop(&mut self, delta: isize) -> Option<&E> {
        let target = self.cursor as isize + delta;
        if target < 0 {
            return None;
        }
        let target = target as usize;
        if target >= self.entries.len() {
            return None;
        }
        self.cursor = target;
        self.entries.get(target)
    }

    /// Retain entries for which `keep` returns true, shifting the cursor down
    /// by the number of removed entries that lay strictly before it so it
    /// stays over the same surviving boundary.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&E) -> bool) {
        let cursor = self.cursor;
        let mut idx = 0;
        let mut removed_before_cursor = 0;
        self.entries.retain(|entry| {
            let kept = keep(entry);
            if !kept && idx < cursor {
                removed_before_cursor += 1;
            }
            idx += 1;
            kept
        });
        self.cursor -= removed_before_cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::NavList;

    fn from_tip(entries: &[u32]) -> NavList<u32> {
        let mut list = NavList::default();
        for &e in entries {
            list.truncate_forward();
            list.push_tip(e);
        }
        list
    }

    #[test]
    fn push_tip_appends_and_parks_at_tip() {
        let list = from_tip(&[10, 20, 30]);
        assert_eq!(list.entries(), &[10, 20, 30]);
        assert_eq!(list.cursor(), 3);
    }

    #[test]
    fn truncate_forward_drops_history_past_cursor() {
        let mut list = from_tip(&[1, 2, 3]);
        list.set_cursor(1);
        list.truncate_forward();
        list.push_tip(9);
        assert_eq!(list.entries(), &[1, 9]);
        assert_eq!(list.cursor(), 2);
    }

    #[test]
    fn step_stop_walks_and_halts_at_edges() {
        let mut list = from_tip(&[1, 2, 3]);
        assert_eq!(list.step_stop(-1), Some(&3));
        assert_eq!(list.step_stop(-1), Some(&2));
        assert_eq!(list.step_stop(-1), Some(&1));
        assert_eq!(list.step_stop(-1), None);
        assert_eq!(list.cursor(), 0);
    }

    #[test]
    fn step_stop_forward_never_reaches_the_tip() {
        let mut list = from_tip(&[1, 2, 3]);
        list.step_stop(-2);
        assert_eq!(list.cursor(), 1);
        assert_eq!(list.step_stop(1), Some(&3));
        assert_eq!(list.step_stop(1), None);
        assert_eq!(list.cursor(), 2);
    }

    #[test]
    fn step_stop_all_or_nothing_by_delta() {
        let mut list = from_tip(&[1, 2, 3]);
        assert_eq!(list.step_stop(-5), None);
        assert_eq!(
            list.cursor(),
            3,
            "an out-of-range step leaves the cursor put"
        );
        assert_eq!(list.step_stop(-2), Some(&2));
    }

    #[test]
    fn set_cursor_clamps_to_tip() {
        let mut list = from_tip(&[1, 2]);
        list.set_cursor(99);
        assert_eq!(list.cursor(), 2);
        list.set_cursor(0);
        assert_eq!(list.cursor(), 0);
    }

    #[test]
    fn pop_front_shifts_cursor_down() {
        let mut list = from_tip(&[1, 2, 3]);
        assert_eq!(list.pop_front(), Some(1));
        assert_eq!(list.entries(), &[2, 3]);
        assert_eq!(list.cursor(), 2);
    }

    #[test]
    fn retain_keeps_cursor_over_surviving_boundary() {
        let mut list = from_tip(&[1, 2, 3, 4, 5]);
        list.set_cursor(4);
        list.retain(|&e| e % 2 == 1);
        assert_eq!(list.entries(), &[1, 3, 5]);
        assert_eq!(
            list.cursor(),
            2,
            "two removed before the cursor shift it down two"
        );
    }

    #[test]
    fn retain_all_removed_leaves_cursor_at_zero() {
        let mut list = from_tip(&[1, 2, 3]);
        list.retain(|_| false);
        assert!(list.entries().is_empty());
        assert_eq!(list.cursor(), 0);
    }
}
