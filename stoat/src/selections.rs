//! Multi-cursor selection management.
//!
//! This module provides efficient multi-cursor support following Zed's architecture.
//! Selections are stored as [`Selection<Anchor>`] for persistence across buffer edits,
//! and lazily resolved to concrete positions when needed for operations or rendering.
//!
//! # Architecture
//!
//! [`SelectionsCollection`] maintains two sets of selections:
//! - `disjoint`: Non-overlapping selections stored in an [`Arc`] slice for cheap cloning
//! - `pending`: An optional temporary selection (e.g., during mouse drag)
//!
//! Selections use [`Anchor`] positions from the `text` crate, which survive buffer
//! edits through timestamped fragment tracking. When positions are needed for operations,
//! anchors are resolved to [`Point`], [`usize`], or other dimensions via [`BufferSnapshot`].
//!
//! # Performance
//!
//! The implementation matches Zed's performance characteristics:
//! - O(1) cloning via [`Arc`]
//! - O(n) overlap merging during selection updates
//! - Lazy anchor resolution
//! - Batch resolution for multiple selections
//!
//! # Related
//!
//! - [`text::Selection`] - Generic selection type from text crate
//! - [`text::Anchor`] - Persistent buffer position
//! - [`text::BufferSnapshot`] - Used to resolve anchors to concrete positions

use std::{cmp::Ordering, sync::Arc};
use text::{Anchor, Bias, BufferSnapshot, Selection, SelectionGoal, ToOffset, ToPoint};

/// Collection of selections with automatic overlap merging.
///
/// Manages multiple cursors efficiently using Zed's architecture. Selections are
/// stored as [`Selection<Anchor>`] which survive buffer edits, then resolved to
/// concrete positions when needed.
///
/// Maintains a `disjoint` set of non-overlapping selections plus an optional `pending`
/// selection for temporary states like mouse dragging. The [`all`](SelectionsCollection::all)
/// method merges pending with disjoint, handling any overlaps.
///
/// # Examples
///
/// ```ignore
/// // Create collection with single cursor at origin
/// let mut selections = SelectionsCollection::new(&buffer_snapshot);
///
/// // Get all selections as Points
/// let points: Vec<Selection<Point>> = selections.all(&buffer_snapshot);
///
/// // Add a new selection
/// selections.select(vec![Selection {
///     id: 1,
///     start: Point::new(0, 0),
///     end: Point::new(0, 5),
///     reversed: false,
///     goal: SelectionGoal::None,
/// }], &buffer_snapshot);
/// ```
///
/// # Related
///
/// Used by [`crate::Stoat`] to manage editor cursors and selections.
#[derive(Debug, Clone)]
pub struct SelectionsCollection {
    /// Non-overlapping selections stored as anchors for persistence.
    ///
    /// Uses [`Arc`] for cheap cloning - common when snapshotting editor state.
    /// Empty only when there's a pending selection covering everything.
    disjoint: Arc<[Selection<Anchor>]>,

    /// Temporary selection, such as during mouse drag.
    ///
    /// When present, [`all`](SelectionsCollection::all) merges this with disjoint selections,
    /// handling any overlaps. This allows temporary selection changes without
    /// modifying the stable `disjoint` set.
    pending: Option<Selection<Anchor>>,

    /// Next unique selection ID.
    ///
    /// Each selection gets a unique ID for tracking across operations. IDs help
    /// determine which selection is "newest" when needed.
    next_selection_id: usize,
}

impl SelectionsCollection {
    /// Create a new selections collection with a single cursor at origin.
    ///
    /// Initializes with one collapsed selection (cursor) at position (0, 0).
    /// The selection uses anchor positions from the provided snapshot.
    ///
    /// # Arguments
    ///
    /// * `buffer` - Buffer snapshot to create initial anchor
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let snapshot = buffer.read(cx).snapshot();
    /// let selections = SelectionsCollection::new(&snapshot);
    /// ```
    pub fn new(buffer: &BufferSnapshot) -> Self {
        let anchor = buffer.anchor_before(0);
        Self {
            disjoint: Arc::new([]),
            pending: Some(Selection {
                id: 0,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }),
            next_selection_id: 1,
        }
    }

    /// Get count of selections (disjoint + pending).
    ///
    /// Returns the total number of cursors/selections including any pending selection.
    pub fn count(&self) -> usize {
        let mut count = self.disjoint.len();
        if self.pending.is_some() {
            count += 1;
        }
        count
    }

    /// Get all selections resolved to a specific dimension type.
    ///
    /// Merges disjoint and pending selections, handling any overlaps, then resolves
    /// anchors to the requested dimension type (typically [`Point`] or `usize`).
    /// Overlapping selections are merged to maintain disjoint property.
    ///
    /// # Type Parameters
    ///
    /// * `D` - Dimension type to resolve to (e.g., [`Point`], `usize`)
    ///
    /// # Arguments
    ///
    /// * `buffer` - Buffer snapshot for anchor resolution
    ///
    /// # Returns
    ///
    /// Vector of non-overlapping selections sorted by position
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let selections: Vec<Selection<Point>> = collection.all(&buffer_snapshot);
    /// for sel in selections {
    ///     println!("Selection from {:?} to {:?}", sel.start, sel.end);
    /// }
    /// ```
    pub fn all<D>(&self, buffer: &BufferSnapshot) -> Vec<Selection<D>>
    where
        D: text::TextDimension + Ord,
    {
        let mut selections = Vec::new();

        // Resolve disjoint selections
        for anchor_sel in self.disjoint.iter() {
            selections.push(resolve_selection(anchor_sel, buffer));
        }

        // Resolve and merge pending if present
        if let Some(pending) = &self.pending {
            let pending_resolved = resolve_selection(pending, buffer);
            merge_selection_into(&mut selections, pending_resolved);
        }

        selections
    }

    /// Get the newest (most recently created) selection as anchors.
    ///
    /// Returns the selection with the highest ID, which is the most recently
    /// created. Prefers pending selection if present, otherwise the newest
    /// from disjoint set.
    ///
    /// # Returns
    ///
    /// Reference to the newest selection
    ///
    /// # Panics
    ///
    /// Panics if there are no selections (this should never happen - there's
    /// always at least a pending selection)
    pub fn newest_anchor(&self) -> &Selection<Anchor> {
        self.pending
            .as_ref()
            .or_else(|| self.disjoint.iter().max_by_key(|s| s.id))
            .expect("SelectionsCollection should always have at least one selection")
    }

    /// Get the newest selection resolved to a specific dimension.
    ///
    /// Resolves the newest selection's anchors to the requested dimension type.
    /// Useful when you only need to operate on the primary/active cursor.
    ///
    /// # Type Parameters
    ///
    /// * `D` - Dimension type to resolve to (e.g., [`Point`], `usize`)
    ///
    /// # Arguments
    ///
    /// * `buffer` - Buffer snapshot for anchor resolution
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let newest: Selection<Point> = selections.newest(&buffer_snapshot);
    /// println!("Primary cursor at {:?}", newest.head());
    /// ```
    pub fn newest<D>(&self, buffer: &BufferSnapshot) -> Selection<D>
    where
        D: text::TextDimension + Ord,
    {
        resolve_selection(self.newest_anchor(), buffer)
    }

    /// Replace all selections with a new set.
    ///
    /// Sorts selections, merges overlapping ones, converts to anchors, and stores
    /// as the new disjoint set. Clears any pending selection. This is the primary
    /// way to update selections after cursor movement or editing operations.
    ///
    /// # Type Parameters
    ///
    /// * `T` - Position type (e.g., [`Point`], `usize`) that can be converted to offset
    ///
    /// # Arguments
    ///
    /// * `selections` - New selections to store (will be merged if overlapping)
    /// * `buffer` - Buffer snapshot for anchor creation
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Move all cursors right
    /// let mut sels: Vec<Selection<Point>> = selections.all(&buffer_snapshot);
    /// for sel in &mut sels {
    ///     sel.start.column += 1;
    ///     sel.end.column += 1;
    /// }
    /// selections.select(sels, &buffer_snapshot);
    /// ```
    pub fn select<T>(&mut self, mut selections: Vec<Selection<T>>, buffer: &BufferSnapshot)
    where
        T: ToOffset + ToPoint + Copy,
    {
        // Sort by start position
        selections.sort_by(|a, b| a.start.to_offset(buffer).cmp(&b.start.to_offset(buffer)));

        // Merge overlapping selections
        let mut i = 1;
        while i < selections.len() {
            let prev_end = selections[i - 1].end.to_offset(buffer);
            let curr_start = selections[i].start.to_offset(buffer);

            if prev_end >= curr_start {
                // Overlapping - merge
                let removed = selections.remove(i);
                let removed_start = removed.start.to_offset(buffer);
                let removed_end = removed.end.to_offset(buffer);
                let prev_start = selections[i - 1].start.to_offset(buffer);
                let prev_end = selections[i - 1].end.to_offset(buffer);

                if removed_start < prev_start {
                    selections[i - 1].start = removed.start;
                }
                if removed_end > prev_end {
                    selections[i - 1].end = removed.end;
                }
            } else {
                i += 1;
            }
        }

        // Convert to anchors
        self.disjoint = selections
            .into_iter()
            .map(|sel| selection_to_anchor_selection(sel, buffer))
            .collect();

        self.pending = None;
    }

    /// Set a single pending selection.
    ///
    /// Used for temporary selection states like mouse dragging. The pending
    /// selection will be merged with disjoint selections when [`all`](SelectionsCollection::all)
    /// is called.
    ///
    /// # Arguments
    ///
    /// * `selection` - Selection to set as pending
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Start mouse drag
    /// selections.set_pending(Selection {
    ///     id: selections.next_id(),
    ///     start: drag_start_anchor,
    ///     end: current_anchor,
    ///     reversed: false,
    ///     goal: SelectionGoal::None,
    /// });
    /// ```
    pub fn set_pending(&mut self, selection: Selection<Anchor>) {
        self.pending = Some(selection);
    }

    /// Clear the pending selection.
    ///
    /// Call this to finalize a pending selection by merging it into the disjoint
    /// set, or to cancel a pending selection.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// Get the next unique selection ID.
    ///
    /// Returns a new unique ID for creating selections. IDs are monotonically
    /// increasing and used to determine selection age/order.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let id = selections.next_id();
    /// let new_selection = Selection {
    ///     id,
    ///     start: anchor1,
    ///     end: anchor2,
    ///     reversed: false,
    ///     goal: SelectionGoal::None,
    /// };
    /// ```
    pub fn next_id(&mut self) -> usize {
        let id = self.next_selection_id;
        self.next_selection_id += 1;
        id
    }

    /// Get the disjoint selections as a cheap [`Arc`] clone.
    ///
    /// Used by the undo system to snapshot selection state before/after edits.
    pub fn disjoint_anchors_arc(&self) -> Arc<[Selection<Anchor>]> {
        if let Some(pending) = &self.pending {
            let mut all = self.disjoint.to_vec();
            all.push(pending.clone());
            Arc::from(all)
        } else {
            self.disjoint.clone()
        }
    }

    /// Restore selections from a saved set of anchor-based selections.
    ///
    /// Replaces the disjoint set and clears any pending selection.
    /// Used by the undo system to restore selection state.
    pub fn select_anchors(&mut self, anchors: Arc<[Selection<Anchor>]>) {
        self.disjoint = anchors;
        self.pending = None;
    }

    /// Try to cancel/reduce selections.
    ///
    /// Implements vim-style selection cancellation:
    /// 1. If pending selection exists, finalize it
    /// 2. If multiple selections, reduce to newest one
    /// 3. If selection has range, collapse to head position
    /// 4. Otherwise, nothing to cancel
    ///
    /// # Arguments
    ///
    /// * `buffer` - Buffer snapshot for position comparison
    ///
    /// # Returns
    ///
    /// `true` if something was cancelled, `false` if nothing to cancel
    ///
    /// # Examples
    ///
    /// ```ignore
    /// if !selections.try_cancel(&buffer_snapshot) {
    ///     // Nothing more to cancel, perhaps exit visual mode
    /// }
    /// ```
    pub fn try_cancel(&mut self, buffer: &BufferSnapshot) -> bool {
        // If pending, finalize it
        if let Some(pending) = self.pending.take() {
            if self.disjoint.is_empty() {
                self.disjoint = Arc::new([pending]);
            }
            return true;
        }

        // If multiple selections, reduce to newest
        if self.count() > 1 {
            let newest = self.newest_anchor().clone();
            self.disjoint = Arc::new([newest]);
            return true;
        }

        // If selection has range, collapse it
        if let Some(selection) = self.disjoint.first() {
            if selection.start.cmp(&selection.end, buffer) != Ordering::Equal {
                let head = if selection.reversed {
                    selection.start
                } else {
                    selection.end
                };
                self.disjoint = Arc::new([Selection {
                    id: selection.id,
                    start: head,
                    end: head,
                    reversed: false,
                    goal: selection.goal,
                }]);
                return true;
            }
        }

        false
    }
}

/// Resolve a selection from anchors to a specific dimension type.
///
/// Converts anchor-based selection to concrete positions. The dimension type
/// determines the output format (e.g., [`Point`] for row/column, `usize` for byte offset).
///
/// # Type Parameters
///
/// * `D` - Target dimension type
///
/// # Arguments
///
/// * `selection` - Selection with anchor positions
/// * `buffer` - Buffer snapshot for resolution
///
/// # Returns
///
/// Selection with positions in dimension D
fn resolve_selection<D>(selection: &Selection<Anchor>, buffer: &BufferSnapshot) -> Selection<D>
where
    D: text::TextDimension + Ord,
{
    Selection {
        id: selection.id,
        start: buffer.summary_for_anchor(&selection.start),
        end: buffer.summary_for_anchor(&selection.end),
        reversed: selection.reversed,
        goal: selection.goal,
    }
}

/// Convert a selection to anchor-based storage.
///
/// Takes a selection with concrete positions (Point, usize, etc.) and converts
/// to anchors for persistent storage. Uses appropriate bias for start/end anchors.
///
/// # Type Parameters
///
/// * `T` - Source position type
///
/// # Arguments
///
/// * `selection` - Selection with concrete positions
/// * `buffer` - Buffer snapshot for anchor creation
///
/// # Returns
///
/// Selection with anchor positions
fn selection_to_anchor_selection<T>(
    selection: Selection<T>,
    buffer: &BufferSnapshot,
) -> Selection<Anchor>
where
    T: ToOffset,
{
    let end_bias = if selection.start.to_offset(buffer) == selection.end.to_offset(buffer) {
        Bias::Right
    } else {
        Bias::Left
    };

    Selection {
        id: selection.id,
        start: buffer.anchor_after(selection.start),
        end: buffer.anchor_at(selection.end, end_bias),
        reversed: selection.reversed,
        goal: selection.goal,
    }
}

/// Merge a selection into a sorted vector, handling overlaps.
///
/// Inserts the selection into the vector at the correct position, merging with
/// any overlapping selections. Maintains the sorted, non-overlapping property.
///
/// # Type Parameters
///
/// * `D` - Position dimension type
///
/// # Arguments
///
/// * `selections` - Sorted vector of non-overlapping selections
/// * `new_selection` - Selection to insert/merge
fn merge_selection_into<D>(selections: &mut Vec<Selection<D>>, mut new_selection: Selection<D>)
where
    D: Ord + Copy,
{
    let insert_pos = selections
        .binary_search_by(|probe| probe.start.cmp(&new_selection.start))
        .unwrap_or_else(|pos| pos);

    // Check for overlaps before insertion point
    let mut merge_start = insert_pos;
    while merge_start > 0 && selections[merge_start - 1].end >= new_selection.start {
        merge_start -= 1;
        if selections[merge_start].start < new_selection.start {
            new_selection.start = selections[merge_start].start;
        }
        if selections[merge_start].end > new_selection.end {
            new_selection.end = selections[merge_start].end;
        }
    }

    // Check for overlaps after insertion point
    let mut merge_end = insert_pos;
    while merge_end < selections.len() && selections[merge_end].start <= new_selection.end {
        if selections[merge_end].end > new_selection.end {
            new_selection.end = selections[merge_end].end;
        }
        merge_end += 1;
    }

    // Remove merged selections and insert new one
    if merge_start < selections.len() {
        selections.drain(merge_start..merge_end);
    }
    selections.insert(merge_start, new_selection);
}
