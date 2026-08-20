use crate::{
    buffer::TextBufferSnapshot,
    display_map::{highlights::HighlightStyle, BlockPlacement, BlockProperties, BlockStyle},
    host::DiffStatus,
    multi_buffer::MultiBufferSnapshot,
};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ops::Range,
    sync::{
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
};
use stoat_text::{Anchor, Bias, ContextLessSummary, Dimension, Item, Point, SeekTarget, SumTree};

static DIFF_MAP_VERSION_COUNTER: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffHunkStatus {
    Added,
    Deleted,
    Modified,
    /// Byte-for-byte equal content that relocated to or from another
    /// position. Paired with provenance in [`TokenDetail`] and
    /// [`ChangeSpan::move_metadata`].
    Moved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Novel,
    Replaced,
    /// Token participates in a move (the containing hunk may still be
    /// [`DiffHunkStatus::Modified`] if neighbouring tokens were edited
    /// rather than moved). The provenance lives on [`ChangeSpan::move_metadata`].
    Moved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSpan {
    pub byte_range: Range<usize>,
    pub kind: ChangeKind,
    pub move_metadata: Option<Arc<stoat_language::structural_diff::MoveMetadata>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDetail {
    pub buffer_spans: Vec<ChangeSpan>,
    pub base_spans: Vec<ChangeSpan>,
}

/// The buffer rows a refined hunk's spans actually touch, as merged runs.
///
/// A hunk's extents stay line-accurate so staging and navigation keep working
/// on whole hunks, but its washes come from the tree differ, which marks
/// tokens rather than lines. Without this the gutter marks all hundred rows of
/// a reindent whose only real change is two tokens.
///
/// `line_of` maps a buffer byte offset to its row. It is a closure because the
/// caller already holds the rope and has to clamp stale offsets against it.
///
/// Runs come back sorted and merged, with adjacent runs joined, so a caller
/// can binary-search them and a full-hunk refinement collapses to one run.
fn marked_row_runs(spans: &[ChangeSpan], line_of: impl Fn(usize) -> u32) -> Vec<Range<u32>> {
    let mut runs: Vec<Range<u32>> = spans
        .iter()
        .filter(|span| !span.byte_range.is_empty())
        .map(|span| {
            let start = line_of(span.byte_range.start);
            // The end is exclusive, so the last byte inside it names the last
            // row. A span ending at a row start must not claim the next row.
            let end = line_of(span.byte_range.end - 1);
            start..end + 1
        })
        .collect();
    runs.sort_by_key(|run| run.start);

    let mut merged: Vec<Range<u32>> = Vec::with_capacity(runs.len());
    for run in runs {
        match merged.last_mut() {
            // Adjacent runs join too. Two spans on consecutive rows describe
            // one marked region, and leaving a seam would let a caller read a
            // gap that is not there.
            Some(last) if run.start <= last.end => last.end = last.end.max(run.end),
            _ => merged.push(run),
        }
    }
    merged
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub status: DiffHunkStatus,
    pub buffer_start_line: u32,
    pub buffer_line_range: Range<u32>,
    pub base_byte_range: Range<usize>,
    pub anchor_range: Option<Range<Anchor>>,
    pub token_detail: Option<Arc<TokenDetail>>,
    /// Buffer-row ranges within this hunk that still differ from the git index.
    ///
    /// Populated by [`DiffMap::from_structural_changes_staged`] from the
    /// index-vs-buffer pass. An empty vec means every changed row is staged. The
    /// full [`Self::buffer_line_range`] means entirely unstaged, the default
    /// from the index-unaware [`DiffMap::from_structural_changes`]. A zero-width
    /// deletion or move hunk stores its anchor point `start..start` when
    /// unstaged. Read through [`Self::staged`] and [`Self::line_staged`], never
    /// as raw ranges.
    pub(crate) unstaged_lines: Vec<Range<u32>>,
    /// Buffer rows inside [`Self::buffer_line_range`] the hunk's structural
    /// spans touch, in the same stored coordinates that range uses.
    ///
    /// Empty means unrefined, and an unrefined hunk marks its whole range.
    /// Only the tree pass fills this. The line differ's spans cover chars on
    /// lines it already called changed, so every row of such a hunk is marked
    /// either way and narrowing costs a walk to reach the same answer.
    ///
    /// Stored rather than resolved per frame because the spans are byte offsets
    /// into the text the diff ran against. Mapping them through the current
    /// rope drifts from the anchor-tracked live rows after any edit above the
    /// hunk.
    pub(crate) marked_rows: Vec<Range<u32>>,
}

impl DiffHunk {
    /// Whether every changed row in this hunk is applied to the git index.
    ///
    /// A partially line-staged hunk keeps its unstaged rows in
    /// [`Self::unstaged_lines`], so it reads as not fully staged.
    pub(crate) fn staged(&self) -> bool {
        self.unstaged_lines.is_empty()
    }

    /// Whether the tree pass narrowed this hunk to particular rows.
    ///
    /// False for a hunk it never reached and for one whose spans it found
    /// nothing in. Both mark their whole range, which is what a caller wants:
    /// an `Added` or `Deleted` hunk never gets structural detail, and a
    /// whitespace-only row deliberately gets an empty one. Neither has rows to
    /// narrow to.
    pub(crate) fn refined(&self) -> bool {
        !self.marked_rows.is_empty()
    }

    /// Whether buffer `row` is applied to the git index.
    ///
    /// Staged when no [`Self::unstaged_lines`] range covers `row`. A zero-width
    /// anchor is matched by [`ranges_overlap`] point semantics.
    fn line_staged(&self, row: u32) -> bool {
        !self
            .unstaged_lines
            .iter()
            .any(|range| ranges_overlap(range, &(row..row + 1)))
    }
}

/// The hunks paired with the buffer rows they now cover, ordered by those rows.
///
/// A hunk stores the rows it covered when the diff ran, and the reader has been
/// typing since. Built by [`DiffMap::live_hunks`], which resolves each hunk's
/// anchors once, so a caller painting a screenful of rows pays for that walk
/// once rather than per row.
pub struct LiveHunks<'a> {
    hunks: Vec<LiveHunk<'a>>,
}

/// One hunk as it sits in the buffer now.
struct LiveHunk<'a> {
    hunk: &'a DiffHunk,
    rows: Range<u32>,
}

impl<'a> LiveHunks<'a> {
    /// The diff mark to paint in the gutter for buffer `line`, or `None` when
    /// no hunk touches it. The row-level counterpart of
    /// [`DiffMap::gutter_mark_for_line`], reading live rows rather than stored
    /// ones.
    ///
    /// A refined hunk marks only the rows its spans touch, so a reindent whose
    /// real change is two tokens paints two marks rather than a hundred. A hunk
    /// with no refinement marks its whole range.
    pub fn gutter_mark_for_line(&self, line: u32) -> Option<(DiffHunkStatus, bool)> {
        let index = self
            .hunks
            .partition_point(|live| live.rows.start <= line)
            .checked_sub(1)?;
        let live = &self.hunks[index];
        let hunk = live.hunk;

        if live.rows.contains(&line) {
            // The staged rows and the marked runs are both recorded in the
            // hunk's own coordinates, so the live row steps back by however far
            // the hunk has moved before either is read.
            let stored = (line + hunk.buffer_line_range.start).saturating_sub(live.rows.start);
            if hunk.refined() && !hunk.marked_rows.iter().any(|run| run.contains(&stored)) {
                return None;
            }
            return Some((hunk.status, hunk.line_staged(stored)));
        }
        if hunk.status == DiffHunkStatus::Deleted && live.rows.start == line {
            return Some((DiffHunkStatus::Deleted, hunk.staged()));
        }
        if hunk.status == DiffHunkStatus::Moved && live.rows.is_empty() && live.rows.start == line {
            return Some((DiffHunkStatus::Moved, hunk.staged()));
        }
        None
    }

    /// The diff status of buffer `line`. The row-level counterpart of
    /// [`DiffMap::status_for_line`], reading live rows rather than stored ones.
    pub fn status_for_line(&self, line: u32) -> DiffStatus {
        let Some(index) = self
            .hunks
            .partition_point(|live| live.rows.start <= line)
            .checked_sub(1)
        else {
            return DiffStatus::Unchanged;
        };
        let live = &self.hunks[index];
        if !live.rows.contains(&line) {
            return DiffStatus::Unchanged;
        }
        match live.hunk.status {
            DiffHunkStatus::Added => DiffStatus::Added,
            DiffHunkStatus::Modified => DiffStatus::Modified,
            DiffHunkStatus::Moved => DiffStatus::Moved,
            DiffHunkStatus::Deleted => DiffStatus::Unchanged,
        }
    }

    /// The row ranges change navigation stops on, in document order.
    ///
    /// A refined hunk contributes one range per marked run, so `n` inside a
    /// hundred-line reindent walks the handful of rows that actually changed
    /// rather than stepping over the whole block in one press. A hunk the tree
    /// pass never narrowed contributes its full live rows, which is the stop it
    /// has always been.
    ///
    /// A zero-width `Deleted` or `Moved` seam keeps its empty range. That is
    /// what a caller turns into a single-cell landing at the seam row.
    pub fn change_stops(&self) -> Vec<Range<u32>> {
        let mut stops = Vec::with_capacity(self.hunks.len());
        for live in &self.hunks {
            match live.hunk.refined() {
                // The runs are the hunk's own coordinates, so they shift with
                // it the same way its live rows did.
                true => {
                    let shift = live.rows.start as i64 - live.hunk.buffer_line_range.start as i64;
                    stops.extend(live.hunk.marked_rows.iter().map(|run| {
                        let start = (run.start as i64 + shift).max(0) as u32;
                        let end = (run.end as i64 + shift).max(0) as u32;
                        start..end
                    }));
                },
                false => stops.push(live.rows.clone()),
            }
        }
        stops.sort_by_key(|run| (run.start, run.end));
        stops
    }
    /// The hunks meeting `rows`, each with the rows it now covers.
    ///
    /// A hunk covering no rows -- a deletion or move seam -- is included when
    /// the row it sits at is in range, since that row is where its mark paints.
    pub fn in_range(&self, rows: Range<u32>) -> impl Iterator<Item = (&'a DiffHunk, Range<u32>)> {
        let from = self
            .hunks
            .partition_point(|live| live.rows.end < rows.start);
        self.hunks[from..]
            .iter()
            .take_while(move |live| live.rows.start < rows.end)
            .map(|live| (live.hunk, live.rows.clone()))
    }
}

// --- SumTree plumbing (follows TreeMap/MapKey pattern from text/src/tree_map.rs) ---

/// SumTree summary for the hunk tree.
///
/// `max_start` carries the largest `buffer_start_line` in a subtree for keyed
/// seeking, so it replaces on combine because hunks are ordered by start.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct HunkKey {
    max_start: Option<u32>,
}

impl ContextLessSummary for HunkKey {
    fn add_summary(&mut self, other: &Self) {
        if other.max_start.is_some() {
            self.max_start = other.max_start;
        }
    }
}

#[derive(Clone, Default, Debug)]
struct HunkKeyRef<'a>(Option<&'a u32>);

impl<'a> Dimension<'a, HunkKey> for HunkKeyRef<'a> {
    fn zero(_cx: ()) -> Self {
        Self(None)
    }
    fn add_summary(&mut self, summary: &'a HunkKey, _cx: ()) {
        self.0 = summary.max_start.as_ref();
    }
}

impl<'a> SeekTarget<'a, HunkKey, HunkKeyRef<'a>> for HunkKeyRef<'_> {
    fn cmp(&self, cursor_location: &HunkKeyRef<'_>, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.0)
    }
}

impl Item for DiffHunk {
    type Summary = HunkKey;
    fn summary(&self, _cx: ()) -> HunkKey {
        HunkKey {
            max_start: Some(self.buffer_start_line),
        }
    }
}

// --- DiffMap ---

/// Syntax highlight spans for the base text, indexed by 0-based base line.
///
/// Each entry holds a line's spans as line-local byte ranges paired with the
/// resolved highlight style, so the diff view's left column can paint base
/// text with tree-sitter token colors.
pub type BaseHighlights = Vec<Vec<(Range<usize>, HighlightStyle)>>;

/// Base-side change spans keyed by 0-based base line, each range line-local
/// within its line and tagged with its [`ChangeKind`], so the diff view's left
/// column can wash each span by kind.
pub(crate) type BaseChangeSpans = BTreeMap<u32, Vec<(Range<usize>, ChangeKind)>>;

type BaseStaged = BTreeMap<u32, bool>;

#[derive(Clone, Debug, Default)]
pub struct DiffMap {
    hunks: SumTree<DiffHunk>,
    base_text: Option<Arc<String>>,
    /// Byte offset of each base line's start, so a line's text is a slice
    /// rather than a walk. Empty when there is no base text.
    ///
    /// Shared behind `Arc` because a snapshot takes a clone of the map and the
    /// table is as long as the base file.
    base_line_starts: Arc<Vec<usize>>,
    base_highlights: Option<Arc<BaseHighlights>>,
    /// Base-side change spans keyed by base line, resolved once at construction
    /// from `hunks` and `base_text` since both are immutable after it. Shared
    /// behind `Arc` so the per-frame accessor hands out a handle instead of
    /// rebuilding the map.
    base_changes: Arc<BaseChangeSpans>,
    /// Git-index staged state keyed by base line, for the diff view's removed
    /// (left-column) rows. Resolved once at construction alongside
    /// [`Self::base_changes`], mapping each base line a hunk removed to that
    /// hunk's [`DiffHunk::staged`]. Added hunks contribute no base line.
    base_staged: Arc<BaseStaged>,
    /// Hunks counted by staged state as `(staged, unstaged)`.
    ///
    /// Kept rather than folded on demand because the status bar asks every
    /// frame while the answer moves only when the diff is rebuilt.
    ///
    /// [`Self::from_hunks`] is the only path that builds the tree outside
    /// tests, since a staging change re-runs the diff rather than editing the
    /// hunks in place. Nothing else takes the tree mutably, so the count cannot
    /// drift from it.
    staged_tally: (usize, usize),
    version: usize,
    /// What [`Self::live_hunks`] last resolved, so the four callers that ask
    /// per frame pay for one walk between them.
    ///
    /// Shared across clones because a [`crate::display_map::DisplaySnapshot`]
    /// takes one and every page paint reads through it. A mutex rather than a
    /// cell for that same reason, a snapshot crossing to a blocking worker.
    resolved_rows: Arc<Mutex<Option<LiveRowsCache>>>,
}

/// The rows [`DiffMap::live_hunks`] resolved for one buffer version.
///
/// Keyed on the buffer alone, the hunks being immutable once the map is built.
/// A rebuilt map starts with none of this, so rows never outlive the hunks they
/// were resolved against.
#[derive(Debug)]
struct LiveRowsCache {
    buffer_version: u64,
    /// One per hunk, in the order the tree iterates them.
    rows: Vec<Range<u32>>,
    /// Indices into `rows`, ordered as the answer is.
    order: Vec<usize>,
}

impl DiffMap {
    fn next_version() -> usize {
        DIFF_MAP_VERSION_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
    }

    pub fn from_hunks(
        hunks: impl IntoIterator<Item = DiffHunk>,
        base_text: Option<Arc<String>>,
    ) -> Self {
        let hunks = SumTree::from_iter(hunks, ());
        let base_changes = Arc::new(compute_base_change_spans(&hunks, base_text.as_ref()));
        let base_staged = Arc::new(compute_base_staged(&hunks, base_text.as_ref()));
        let staged_tally = hunks.iter().fold((0, 0), |(staged, unstaged), hunk| {
            if hunk.staged() {
                (staged + 1, unstaged)
            } else {
                (staged, unstaged + 1)
            }
        });

        let base_line_starts = Arc::new(
            base_text
                .as_ref()
                .map(|text| line_starts(text))
                .unwrap_or_default(),
        );

        Self {
            hunks,
            base_text,
            base_line_starts,
            base_highlights: None,
            base_changes,
            base_staged,
            staged_tally,
            version: Self::next_version(),
            resolved_rows: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach base-text syntax highlights for the diff view's left column.
    pub fn set_base_highlights(&mut self, highlights: Arc<BaseHighlights>) {
        self.base_highlights = Some(highlights);
    }

    /// Pin every hunk's buffer rows to `snapshot`, so a later reader can find
    /// where those rows have moved to.
    ///
    /// A hunk's stored rows are where it sat when the diff was computed. The
    /// next diff is a background job away, and the reader keeps typing in the
    /// meantime, so the rows alone go stale the moment the buffer moves.
    ///
    /// `snapshot` must be the text the diff was computed against. Anchoring
    /// against a later one would pin rows the diff never looked at.
    ///
    /// A hunk covering the last line of a file with no trailing newline ends at
    /// the end of the text. The only anchor there follows every later append.
    /// Text typed at the end of such a file joins the hunk until the next diff
    /// redraws the boundary. The alternative loses the hunk's mark on that line
    /// entirely, so the drift is the better trade.
    pub fn anchor_hunks(&mut self, snapshot: &TextBufferSnapshot) {
        if self.hunks.is_empty() {
            return;
        }

        let rope = &snapshot.visible_text;
        let max_row = rope.max_point().row;
        let start_offsets: Vec<usize> = self
            .hunks
            .iter()
            .map(|hunk| {
                rope.point_to_offset(Point::new(hunk.buffer_line_range.start.min(max_row), 0))
            })
            .collect();
        let end_offsets: Vec<usize> = self
            .hunks
            .iter()
            .zip(&start_offsets)
            .map(|(hunk, &start)| {
                if hunk.buffer_line_range.is_empty() {
                    // A zero-width deletion or move hunk covers no rows, and its
                    // row is clamped into the text where the start already sits.
                    // An end of its own reaches the end of the text instead, and
                    // gives the hunk a row the diff never found.
                    return start;
                }
                // A row past the last one answers the rope's length, which is
                // how an end reaches past an unterminated final line.
                rope.point_to_offset(Point::new(hunk.buffer_line_range.end, 0))
            })
            .collect();

        // Left for the start and right for the end, so text inserted at either
        // edge falls outside the hunk rather than silently joining it.
        let starts = snapshot.anchors_at_batch(&start_offsets[..], Bias::Left);
        let ends = snapshot.anchors_at_batch(&end_offsets[..], Bias::Right);

        let anchored: Vec<DiffHunk> = self
            .hunks
            .iter()
            .enumerate()
            .map(|(i, hunk)| DiffHunk {
                anchor_range: Some(starts[i]..ends[i]),
                ..hunk.clone()
            })
            .collect();
        self.hunks = SumTree::from_iter(anchored, ());
    }

    /// Syntax highlight spans for base `line`, or `None` when the base text was
    /// not highlighted (no language) or the line is out of range.
    pub fn base_highlights_for_line(&self, line: u32) -> Option<&[(Range<usize>, HighlightStyle)]> {
        self.base_highlights
            .as_ref()?
            .get(line as usize)
            .map(Vec::as_slice)
    }

    /// Build a [`DiffMap`] from a structural-diff result.
    ///
    /// `lhs` is the base content the diff was computed against and `rhs_text`
    /// is the buffer content.
    ///
    /// The base arrives by handle rather than by slice, so the map shares the
    /// caller's copy instead of taking one of its own. That is also what lets a
    /// later recompute recognise an unchanged base without reading it.
    ///
    /// Adjacent Lhs+Rhs runs from the diff become
    /// [`DiffHunkStatus::Modified`] hunks. An isolated run becomes
    /// [`DiffHunkStatus::Added`] (Rhs only) or [`DiffHunkStatus::Deleted`]
    /// (Lhs only). The conversion preserves the original byte ranges so the
    /// structural-diff sub-line spans remain available via
    /// [`DiffHunk::token_detail`] in a follow-up.
    pub fn from_structural_changes(
        result: stoat_language::structural_diff::DiffResult,
        lhs: Arc<String>,
        rhs_text: &str,
    ) -> Self {
        let hunks = changes_to_hunks(&result.changes, &lhs, rhs_text);
        Self::from_hunks(hunks, Some(lhs))
    }

    /// Build a [`DiffMap`] like [`Self::from_structural_changes`], additionally
    /// marking each hunk whose change is already applied to the git index.
    ///
    /// `index_changed` is the set of buffer-line ranges that differ between the
    /// index and the buffer, from a `structural_diff(index, buffer)` pass. A
    /// hunk is staged when no such range overlaps its `buffer_line_range`,
    /// because the index and buffer already agree over the hunk's extent.
    pub fn from_structural_changes_staged(
        result: stoat_language::structural_diff::DiffResult,
        lhs: Arc<String>,
        rhs_text: &str,
        index_changed: &[Range<u32>],
    ) -> Self {
        let mut hunks = changes_to_hunks(&result.changes, &lhs, rhs_text);
        mark_staged(&mut hunks, index_changed);
        Self::from_hunks(hunks, Some(lhs))
    }

    /// The hunks as they sit in `buffer`, for a caller reading many rows.
    ///
    /// Resolving every hunk's anchors costs one ordered walk of the buffer's
    /// fragments. Reading each row against the stored rows instead costs
    /// nothing, but answers for the text as it was when the diff ran.
    pub fn live_hunks<'a>(&'a self, buffer: &MultiBufferSnapshot) -> LiveHunks<'a> {
        let buffer_version = buffer.version();
        let mut cache = self.resolved_rows.lock().expect("poisoned");

        // Four callers ask per frame and the answer moves only with the buffer,
        // so the first one through pays the walk and the rest read it back.
        if let Some(held) = cache
            .as_ref()
            .filter(|held| held.buffer_version == buffer_version)
        {
            let hunks: Vec<&DiffHunk> = self.hunks.iter().collect();
            return LiveHunks {
                hunks: held
                    .order
                    .iter()
                    .map(|&i| LiveHunk {
                        hunk: hunks[i],
                        rows: held.rows[i].clone(),
                    })
                    .collect(),
            };
        }

        // A hunk built before anchoring, or by a caller that never anchored,
        // keeps its stored rows. Stale, but the only answer available for it.
        let mut live: Vec<(&DiffHunk, Range<u32>)> = self
            .hunks
            .iter()
            .map(|hunk| (hunk, hunk.buffer_line_range.clone()))
            .collect();

        let mut anchors = Vec::with_capacity(live.len() * 2);
        let mut anchored: Vec<usize> = Vec::with_capacity(live.len());
        for (i, hunk) in self.hunks.iter().enumerate() {
            if let Some(range) = &hunk.anchor_range {
                anchored.push(i);
                anchors.push(range.start);
                anchors.push(range.end);
            }
        }

        if !anchors.is_empty() {
            let offsets = buffer.resolve_anchors_batch(&anchors);
            let points = buffer.rope().offsets_to_points_batch(&offsets);
            for (slot, &i) in anchored.iter().enumerate() {
                let start = points[slot * 2];
                let end = points[slot * 2 + 1];
                // An end resting inside a row covers that row. The last line of
                // a file with no trailing newline has no row start after it to
                // hold an exclusive end.
                live[i].1 = start.row..end.row + u32::from(end.column > 0);
            }
        }

        // Held in hunk order with the answer's order beside it, so a later call
        // rebuilds the answer without resolving or sorting again.
        let rows: Vec<Range<u32>> = live.iter().map(|(_, rows)| rows.clone()).collect();
        let mut order: Vec<usize> = (0..rows.len()).collect();
        // An edit inside one hunk moves it past another only if the diff itself
        // is stale enough to have overlapping hunks, so this rarely reorders.
        order.sort_by_key(|&i| (rows[i].start, rows[i].end));

        let held = cache.insert(LiveRowsCache {
            buffer_version,
            rows,
            order,
        });
        LiveHunks {
            hunks: held
                .order
                .iter()
                .map(|&i| LiveHunk {
                    hunk: live[i].0,
                    rows: held.rows[i].clone(),
                })
                .collect(),
        }
    }

    pub fn version(&self) -> usize {
        self.version
    }

    /// Whether `other` would decorate the buffer exactly as this map does.
    ///
    /// Compares the hunks and the base text, which is everything rendering
    /// reads. The base change spans, the staged map, and the staged tally are
    /// all derived from those two at construction, and the base highlights from
    /// the base text and the style table.
    ///
    /// Deliberately not the version. A version is minted per construction, and
    /// preserving it across a recompute that changed nothing is the whole point
    /// of asking.
    pub(crate) fn renders_same_as(&self, other: &Self) -> bool {
        let same_base = match (&self.base_text, &other.base_text) {
            // The ordinary path. A recompute takes its base from the same
            // handle the last one did, so the bases are the same allocation
            // and reading them would be a memcmp of the file for an answer
            // the pointers already give.
            (Some(mine), Some(theirs)) => Arc::ptr_eq(mine, theirs) || mine == theirs,
            (None, None) => true,
            _ => false,
        };
        same_base && self.hunks.iter().eq(other.hunks.iter())
    }

    pub fn base_text(&self) -> Option<&Arc<String>> {
        self.base_text.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn status_for_line(&self, line: u32) -> DiffStatus {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        match cursor.item() {
            Some(hunk) if hunk.buffer_line_range.contains(&line) => match hunk.status {
                DiffHunkStatus::Added => DiffStatus::Added,
                DiffHunkStatus::Modified => DiffStatus::Modified,
                DiffHunkStatus::Moved => DiffStatus::Moved,
                DiffHunkStatus::Deleted => DiffStatus::Unchanged,
            },
            _ => DiffStatus::Unchanged,
        }
    }

    /// The diff mark to paint in the gutter for buffer `line`, or `None` when no
    /// hunk touches it.
    ///
    /// A row inside a hunk's `buffer_line_range` reports that hunk's status. A
    /// row a [`DiffHunkStatus::Deleted`] hunk anchors -- its removed content
    /// rendered just above -- reports `Deleted`, the deletion seam. The bool is
    /// the row's git-index staged state for a contained row, or the whole
    /// hunk's for a deletion or move seam.
    ///
    /// Reports whole hunk ranges. The live counterpart
    /// [`LiveHunks::gutter_mark_for_line`] narrows a refined hunk to the rows
    /// its runs name, which is what the render paths read.
    pub fn gutter_mark_for_line(&self, line: u32) -> Option<(DiffHunkStatus, bool)> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        let hunk = cursor.item()?;
        if hunk.buffer_line_range.contains(&line) {
            return Some((hunk.status, hunk.line_staged(line)));
        }
        if hunk.status == DiffHunkStatus::Deleted && hunk.buffer_start_line == line {
            return Some((DiffHunkStatus::Deleted, hunk.staged()));
        }
        if hunk.status == DiffHunkStatus::Moved
            && hunk.buffer_line_range.is_empty()
            && hunk.buffer_start_line == line
        {
            return Some((DiffHunkStatus::Moved, hunk.staged()));
        }
        None
    }

    /// The git-index staged state of the hunk containing `line`, or `None`
    /// when no hunk covers it.
    ///
    /// `Some(true)` marks a hunk already applied to the index, `Some(false)`
    /// an unstaged one. Deletion hunks occupy no buffer rows, so no line
    /// resolves to one here.
    pub fn staged_for_line(&self, line: u32) -> Option<bool> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        cursor
            .item()
            .filter(|hunk| hunk.buffer_line_range.contains(&line))
            .map(|hunk| hunk.line_staged(line))
    }

    /// Count hunks by staged state as `(staged, unstaged)` for a statusline.
    ///
    /// A partially line-staged hunk counts as unstaged, so the statusline
    /// reports it staged only once every changed row is in the index.
    pub fn staged_counts(&self) -> (usize, usize) {
        self.staged_tally
    }

    /// Whether a block of removed base lines sits directly below `line`.
    ///
    /// The two kinds of block land in different places, so this asks after
    /// both. A hunk with no live rows to pair against blocks above the row it
    /// was removed before, which is the row after this one. A Modified hunk
    /// pairs its base rows with its live ones and blocks only what is left
    /// over, after its own last live row, so it answers here when that row is
    /// this one and its base outruns its live side.
    pub fn has_deletion_after(&self, line: u32, pair_modified: bool) -> bool {
        let after = line + 1;
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&HunkKeyRef(Some(&after)), Bias::Left);
        if let Some(hunk) = cursor.item()
            && hunk.buffer_start_line == after
            && !hunk.base_byte_range.is_empty()
            && match hunk.status {
                DiffHunkStatus::Deleted => true,
                DiffHunkStatus::Modified => self.paired_rows(hunk, pair_modified) == 0,
                _ => false,
            }
        {
            return true;
        }

        // A Modified hunk's block hangs off its last live row, so the hunk that
        // could answer starts at or before this row rather than after it.
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&HunkKeyRef(Some(&line)), Bias::Right);
        cursor.prev();
        cursor.item().is_some_and(|hunk| {
            hunk.status == DiffHunkStatus::Modified
                && !hunk.base_byte_range.is_empty()
                && hunk.buffer_line_range.end == after
                && self.paired_rows(hunk, pair_modified) > 0
                && self.base_line_count(hunk) > self.paired_rows(hunk, pair_modified)
        })
    }

    /// Buffer rows before `buffer_row` that have no base line beside them.
    ///
    /// A row is base-present when the left column paints something on it: an
    /// unchanged row mirrors its base line, and a modified row is paired with
    /// one for as far as the hunk's base text reaches. Past that the hunk's
    /// live rows outrun its base rows and the left column is blank, and an
    /// added hunk has no base rows at all.
    ///
    /// Lets the diff view map a viewport top to its base line. A hunk walk
    /// rather than a tree dimension, since the count depends on each hunk's
    /// base line count, which is not in the summary, and a file's hunks are
    /// few.
    pub fn rows_without_base_before(&self, buffer_row: u32, pair_modified: bool) -> u32 {
        self.hunks
            .iter()
            .take_while(|hunk| hunk.buffer_start_line < buffer_row)
            .map(|hunk| {
                let rows = hunk
                    .buffer_line_range
                    .end
                    .min(buffer_row)
                    .saturating_sub(hunk.buffer_start_line);
                match hunk.status {
                    DiffHunkStatus::Added => rows,
                    DiffHunkStatus::Modified if pair_modified => {
                        rows.saturating_sub(self.base_line_count(hunk))
                    },
                    DiffHunkStatus::Modified => rows,
                    DiffHunkStatus::Deleted | DiffHunkStatus::Moved => 0,
                }
            })
            .sum()
    }

    /// One base line's text, with its trailing newline excluded.
    ///
    /// `None` past the end of the base text, and for a map with none at all,
    /// which is what a caller painting a row beyond the base file sees.
    pub(crate) fn base_line_text(&self, line: u32) -> Option<&str> {
        let text = self.base_text.as_ref()?;
        let start = *self.base_line_starts.get(line as usize)?;
        let end = self
            .base_line_starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or(text.len());
        Some(text[start..end].trim_end_matches('\n'))
    }

    /// Base lines `hunk` removed, which is how many of its live rows have a
    /// base row to pair with.
    ///
    /// Zero without base text, since nothing can be paired against text the map
    /// does not hold.
    pub(crate) fn base_line_count(&self, hunk: &DiffHunk) -> u32 {
        let Some(text) = self.base_text.as_ref() else {
            return 0;
        };
        text[hunk.base_byte_range.clone()].lines().count() as u32
    }

    /// What [`Self::deleted_blocks`] would produce, reduced to what tells one
    /// refresh's set apart from the next.
    ///
    /// A diff recompute stamps a new version whether or not any hunk moved, so
    /// a caller that re-splices on the version alone re-splices constantly.
    /// Comparing the blocks themselves is not open to it, since
    /// [`BlockProperties`] carries a render closure. These four fields are
    /// every input the blocks are built from, so equal signatures mean an
    /// identical set. The live range's end is among them because a Modified
    /// hunk pairs its base rows against its live ones, so a hunk that kept its
    /// start and its base bytes still blocks differently once its live row
    /// count moves.
    pub fn deleted_block_signature(
        &self,
        pair_modified: bool,
    ) -> Vec<(DiffHunkStatus, u32, u32, Range<usize>)> {
        if self.base_text.is_none() {
            return Vec::new();
        }
        self.deleted_block_hunks(pair_modified)
            .map(|hunk| {
                (
                    hunk.status,
                    hunk.buffer_start_line,
                    hunk.buffer_line_range.end,
                    hunk.base_byte_range.clone(),
                )
            })
            .collect()
    }

    /// The hunks that render as deleted-line blocks, shared by
    /// [`Self::deleted_blocks`] and [`Self::deleted_block_signature`] so the
    /// signature cannot drift from the set it stands for.
    ///
    /// A hunk whose base rows all pair with live rows is not among them: the
    /// paint puts those in the left column of the rows they pair with, leaving
    /// nothing for a block to hold.
    fn deleted_block_hunks(&self, pair_modified: bool) -> impl Iterator<Item = &DiffHunk> {
        self.hunks.iter().filter(move |hunk| {
            matches!(
                hunk.status,
                DiffHunkStatus::Deleted | DiffHunkStatus::Modified
            ) && !hunk.base_byte_range.is_empty()
                && self.base_line_count(hunk) > self.paired_rows(hunk, pair_modified)
        })
    }

    /// How many of `hunk`'s base rows the paint puts beside a live row.
    ///
    /// A Modified hunk pairs base row i with live row i as far as the shorter
    /// side reaches. Nothing else pairs: a Deleted hunk has no live rows of its
    /// own, and an Added one no base rows.
    ///
    /// `pair_modified` is off for the unified layout, which has one column and
    /// so nowhere to put a base row beside a live one. There every base row
    /// blocks, and the two sides read as a stacked diff.
    fn paired_rows(&self, hunk: &DiffHunk, pair_modified: bool) -> u32 {
        match hunk.status {
            DiffHunkStatus::Modified if pair_modified => hunk.buffer_line_range.len() as u32,
            _ => 0,
        }
    }

    /// The base lines that have no live row to sit beside, as blocks.
    ///
    /// A Modified hunk's base rows are paired with its live rows one for one as
    /// far as the shorter side reaches, and the paint puts them in the left
    /// column of those rows. Only the excess needs a row of its own, and it
    /// goes after the hunk's last live row, so the filler a length difference
    /// costs lands at the hunk's end rather than in the middle of it. A hunk
    /// whose base is no longer than its live side yields no block at all.
    ///
    /// A Deleted hunk has no live rows to pair with, so all of it blocks, above
    /// the row it was deleted before.
    pub fn deleted_blocks(&self, pair_modified: bool) -> Vec<BlockProperties> {
        let base_text = match &self.base_text {
            Some(t) => t,
            None => return Vec::new(),
        };

        self.deleted_block_hunks(pair_modified)
            .map(|hunk| {
                let paired = self.paired_rows(hunk, pair_modified);
                let content = &base_text[hunk.base_byte_range.clone()];
                let lines: Vec<String> = content
                    .lines()
                    .skip(paired as usize)
                    .map(String::from)
                    .collect();

                let placement_line = match hunk.status {
                    DiffHunkStatus::Modified if paired > 0 => hunk.buffer_line_range.end - 1,
                    _ => hunk.buffer_start_line.saturating_sub(1),
                };
                let mut props = BlockProperties::from_text(
                    BlockPlacement::Below(placement_line),
                    lines,
                    BlockStyle::Fixed,
                );
                props.diff_status = Some(hunk.status);
                props
            })
            .collect()
    }

    /// All hunks in buffer-start order.
    ///
    /// Unlike [`Self::hunks_in_range`], this includes moved-away seam hunks whose
    /// `buffer_line_range` is empty and so match no line range.
    pub fn hunks(&self) -> impl Iterator<Item = &DiffHunk> {
        self.hunks.iter()
    }

    pub fn hunks_in_range(&self, line_range: Range<u32>) -> Vec<&DiffHunk> {
        let mut result = Vec::new();
        self.hunks_in_range_into(line_range, &mut result);
        result
    }

    /// Collect the hunks overlapping `line_range` into `out`, replacing what it
    /// held.
    ///
    /// For callers seeking once per painted row, where allocating a vector per
    /// row is the cost rather than the seek. Everyone else wants
    /// [`Self::hunks_in_range`].
    pub fn hunks_in_range_into<'a>(&'a self, line_range: Range<u32>, out: &mut Vec<&'a DiffHunk>) {
        out.clear();

        let target = HunkKeyRef(Some(&line_range.start));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        // Check if the hunk before the target overlaps
        if let Some(hunk) = cursor.item()
            && hunk.buffer_line_range.end > line_range.start
        {
            out.push(hunk);
        }
        cursor.next();
        while let Some(hunk) = cursor.item() {
            if hunk.buffer_start_line >= line_range.end {
                break;
            }
            out.push(hunk);
            cursor.next();
        }
    }

    pub fn token_detail_for_line(&self, line: u32) -> Option<&TokenDetail> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        match cursor.item() {
            Some(hunk) if hunk.buffer_line_range.contains(&line) => hunk.token_detail.as_deref(),
            _ => None,
        }
    }

    /// Base-side change spans to wash in the diff view's left column, keyed by
    /// base line with each range line-local within that line and tagged with its
    /// [`ChangeKind`].
    ///
    /// Distributes every hunk's [`TokenDetail::base_spans`] -- absolute byte
    /// ranges in the base text -- across the base lines they cover, so a deleted
    /// or modified base block row washes only its changed chars, keyed added /
    /// removed / moved by kind. Empty when the map carries no base text.
    pub(crate) fn base_change_spans(&self) -> Arc<BaseChangeSpans> {
        self.base_changes.clone()
    }

    /// The git-index staged state of the hunk that removed content at base
    /// `line`, or `None` when no hunk did.
    ///
    /// Serves the diff view's removed left-column rows, whose base lines have no
    /// buffer counterpart for [`Self::staged_for_line`] to resolve.
    pub(crate) fn base_line_staged(&self, line: u32) -> Option<bool> {
        self.base_staged.get(&line).copied()
    }

    pub fn total_deleted_lines(&self) -> u32 {
        let base_text = match &self.base_text {
            Some(t) => t,
            None => return 0,
        };
        self.hunks
            .iter()
            .filter(|h| {
                matches!(h.status, DiffHunkStatus::Deleted | DiffHunkStatus::Modified)
                    && !h.base_byte_range.is_empty()
            })
            .map(|h| {
                let content = &base_text[h.base_byte_range.clone()];
                content.lines().count() as u32
            })
            .sum()
    }

    #[cfg(test)]
    pub fn set_base_text(&mut self, text: Arc<String>) {
        self.base_text = Some(text);
        self.base_changes = Arc::new(compute_base_change_spans(
            &self.hunks,
            self.base_text.as_ref(),
        ));
        self.base_staged = Arc::new(compute_base_staged(&self.hunks, self.base_text.as_ref()));
        self.version = Self::next_version();
    }

    #[cfg(test)]
    pub fn push_hunk(&mut self, hunk: DiffHunk) {
        if hunk.staged() {
            self.staged_tally.0 += 1;
        } else {
            self.staged_tally.1 += 1;
        }

        self.hunks.push(hunk, ());
        self.base_changes = Arc::new(compute_base_change_spans(
            &self.hunks,
            self.base_text.as_ref(),
        ));
        self.base_staged = Arc::new(compute_base_staged(&self.hunks, self.base_text.as_ref()));
        self.version = Self::next_version();
    }
}

/// Convert a structural-diff change list into [`DiffHunk`]s.
///
/// The structural path emits per-atom `DiffChange` entries each with
/// its own `kind`; this pass groups them back into hunks. Adjacent
/// Lhs+Rhs Novel/Replaced runs collapse into a [`DiffHunkStatus::Modified`]
/// hunk; isolated Novel runs become Added or Deleted; `Moved` changes
/// become [`DiffHunkStatus::Moved`] hunks whose [`TokenDetail`] carries
/// the per-atom [`ChangeSpan`]s and the shared [`MoveMetadata`] so the
/// renderer can style the subtree and the action layer can jump to
/// the counterpart location(s).
///
/// Moved DiffChanges with the same `Arc<MoveMetadata>` are coalesced
/// into one hunk regardless of side: byte-adjacency does not matter
/// because the metadata Arc identifies the move root. On each side
/// we emit one [`TokenDetail::buffer_spans`] / `base_spans` entry per
/// atom so downstream rendering can style each token independently.
/// Whether two buffer-line ranges intersect.
///
/// An empty range (a deletion, which occupies no buffer rows) is treated as
/// its anchor point, so a deletion hunk still matches an index change touching
/// that point. Non-empty ranges use standard half-open overlap.
fn ranges_overlap(a: &Range<u32>, b: &Range<u32>) -> bool {
    if a.start == a.end || b.start == b.end {
        a.start <= b.end && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// Record which of each hunk's rows still differ from the git index.
///
/// `index_changed` is the buffer-row set an index-vs-buffer diff reported, so a
/// row absent from it is already applied to the index. A zero-width hunk is a
/// deletion seam or a move anchor and has no rows to intersect, so it stores
/// its anchor point when an index change touches it and nothing when not.
pub(crate) fn mark_staged(hunks: &mut [DiffHunk], index_changed: &[Range<u32>]) {
    for hunk in hunks.iter_mut() {
        let range = hunk.buffer_line_range.clone();
        if range.start == range.end {
            let unstaged = index_changed.iter().any(|c| ranges_overlap(c, &range));
            hunk.unstaged_lines = match unstaged {
                true => vec![range],
                false => Vec::new(),
            };
        } else {
            hunk.unstaged_lines = index_changed
                .iter()
                .filter(|c| ranges_overlap(c, &range))
                .map(|c| c.start.max(range.start)..c.end.min(range.end))
                .collect();
        }
    }
}
/// Fold a structural diff's changes into hunks, in buffer-line order.
///
/// Also reachable on its own for a caller that wants the hunks and nothing
/// else. Going through [`DiffMap::from_structural_changes`] to read them back
/// would copy `lhs_text` and build the base change-span and staged maps, all of
/// which such a caller drops.
pub(crate) fn changes_to_hunks(
    changes: &[stoat_language::structural_diff::DiffChange],
    lhs_text: &str,
    rhs_text: &str,
) -> Vec<DiffHunk> {
    use std::collections::HashMap;
    use stoat_language::structural_diff::{ChangeKind as LangChangeKind, Side};

    let lhs_starts = line_starts(lhs_text);
    let rhs_starts = line_starts(rhs_text);

    // Group Moved changes by their shared MoveMetadata Arc. Each group
    // becomes one DiffHunk (one per side, since a move has both an
    // LHS source subtree and an RHS target subtree).
    let mut move_groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, change) in changes.iter().enumerate() {
        if let (LangChangeKind::Moved, Some(meta)) = (&change.kind, &change.move_metadata) {
            let key = Arc::as_ptr(meta) as usize;
            move_groups.entry(key).or_default().push(idx);
        }
    }

    let mut hunks = Vec::new();
    let mut consumed = vec![false; changes.len()];

    // Emit Moved hunks first, one per (Arc identity, side) pair.
    for indices in move_groups.values() {
        let mut lhs_indices: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|i| changes[*i].side == Side::Lhs)
            .collect();
        let mut rhs_indices: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|i| changes[*i].side == Side::Rhs)
            .collect();
        lhs_indices.sort_by_key(|i| changes[*i].byte_range.start);
        rhs_indices.sort_by_key(|i| changes[*i].byte_range.start);

        let metadata = indices
            .iter()
            .filter_map(|i| changes[*i].move_metadata.clone())
            .next();

        if !rhs_indices.is_empty() {
            let first = &changes[*rhs_indices
                .first()
                .expect("rhs_indices non-empty per enclosing guard")];
            let last = &changes[*rhs_indices
                .last()
                .expect("rhs_indices non-empty per enclosing guard")];
            let full_range = first.byte_range.start..last.byte_range.end;
            let line_range = byte_range_to_line_range(&rhs_starts, rhs_text.len(), &full_range);
            let base_range = if let (Some(&lhs_first), Some(&lhs_last)) =
                (lhs_indices.first(), lhs_indices.last())
            {
                changes[lhs_first].byte_range.start..changes[lhs_last].byte_range.end
            } else if let Some(meta) = &metadata {
                // No LHS-side Moved changes in this group? Fall back
                // to the first metadata source's byte range so the
                // hunk can still surface the counterpart location.
                meta.sources
                    .first()
                    .map(|s| s.byte_range.clone())
                    .unwrap_or(0..0)
            } else {
                0..0
            };
            let buffer_spans = rhs_indices
                .iter()
                .map(|i| ChangeSpan {
                    byte_range: changes[*i].byte_range.clone(),
                    kind: ChangeKind::Moved,
                    move_metadata: metadata.clone(),
                })
                .collect();
            let base_spans = lhs_indices
                .iter()
                .map(|i| ChangeSpan {
                    byte_range: changes[*i].byte_range.clone(),
                    kind: ChangeKind::Moved,
                    move_metadata: metadata.clone(),
                })
                .collect();
            hunks.push(DiffHunk {
                status: DiffHunkStatus::Moved,
                unstaged_lines: Vec::new(),
                marked_rows: Vec::new(),
                buffer_start_line: line_range.start,
                buffer_line_range: line_range,
                base_byte_range: base_range,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans,
                    base_spans,
                })),
            });
            for i in &rhs_indices {
                consumed[*i] = true;
            }
            for i in &lhs_indices {
                consumed[*i] = true;
            }
        } else if !lhs_indices.is_empty() {
            // LHS-only move: the source side of a 1:N duplication.
            // Emit a Deleted-style placeholder at the LHS line so
            // the source can still be highlighted / jumped to.
            let first = &changes[*lhs_indices
                .first()
                .expect("lhs_indices non-empty per enclosing else-if guard")];
            let last = &changes[*lhs_indices
                .last()
                .expect("lhs_indices non-empty per enclosing else-if guard")];
            let full_range = first.byte_range.start..last.byte_range.end;
            let lhs_line = line_of(&lhs_starts, first.byte_range.start);
            let base_spans = lhs_indices
                .iter()
                .map(|i| ChangeSpan {
                    byte_range: changes[*i].byte_range.clone(),
                    kind: ChangeKind::Moved,
                    move_metadata: metadata.clone(),
                })
                .collect();
            hunks.push(DiffHunk {
                status: DiffHunkStatus::Moved,
                unstaged_lines: Vec::new(),
                marked_rows: Vec::new(),
                buffer_start_line: lhs_line,
                buffer_line_range: lhs_line..lhs_line,
                base_byte_range: full_range,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: Vec::new(),
                    base_spans,
                })),
            });
            for i in &lhs_indices {
                consumed[*i] = true;
            }
        }
    }

    // Group Lhs/Rhs Replaced changes by pair_id so interleaved orderings
    // collapse into one Modified hunk keyed on the stable pair identifier
    // rather than positional adjacency.
    let mut by_pair: HashMap<u32, (Option<usize>, Option<usize>)> = HashMap::new();
    for (idx, change) in changes.iter().enumerate() {
        if consumed[idx] {
            continue;
        }
        if change.kind == LangChangeKind::Moved {
            continue;
        }
        if let Some(pair) = change.pair_id {
            let slot = by_pair.entry(pair).or_default();
            match change.side {
                Side::Lhs => slot.0 = Some(idx),
                Side::Rhs => slot.1 = Some(idx),
            }
        }
    }
    for (lhs_idx, rhs_idx) in by_pair.values().filter_map(|p| Some((p.0?, p.1?))) {
        let lhs_change = &changes[lhs_idx];
        let rhs_change = &changes[rhs_idx];
        let line_range =
            byte_range_to_line_range(&rhs_starts, rhs_text.len(), &rhs_change.byte_range);
        hunks.push(DiffHunk {
            status: DiffHunkStatus::Modified,
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: lhs_change.byte_range.clone(),
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: replaced_change_spans(rhs_change),
                base_spans: replaced_change_spans(lhs_change),
            })),
        });
        consumed[lhs_idx] = true;
        consumed[rhs_idx] = true;
    }

    for (idx, cur) in changes.iter().enumerate() {
        if consumed[idx] {
            continue;
        }
        match cur.side {
            Side::Rhs => {
                let line_range =
                    byte_range_to_line_range(&rhs_starts, rhs_text.len(), &cur.byte_range);
                hunks.push(DiffHunk {
                    status: DiffHunkStatus::Added,
                    unstaged_lines: Vec::new(),
                    marked_rows: Vec::new(),
                    buffer_start_line: line_range.start,
                    buffer_line_range: line_range,
                    base_byte_range: 0..0,
                    anchor_range: None,
                    token_detail: None,
                });
            },
            Side::Lhs => {
                // Prefer the rhs anchor emitted by the structural-diff layer
                // so deletions display between their surrounding rhs lines.
                // Fall back to the lhs-line index when the diff producer did
                // not supply one (e.g. tree-diff path for now).
                let buffer_line = cur
                    .deletion_rhs_anchor
                    .unwrap_or_else(|| line_of(&lhs_starts, cur.byte_range.start));
                hunks.push(DiffHunk {
                    status: DiffHunkStatus::Deleted,
                    unstaged_lines: Vec::new(),
                    marked_rows: Vec::new(),
                    buffer_start_line: buffer_line,
                    buffer_line_range: buffer_line..buffer_line,
                    base_byte_range: cur.byte_range.clone(),
                    anchor_range: None,
                    token_detail: None,
                });
            },
        }
    }
    for hunk in &mut hunks {
        hunk.unstaged_lines = vec![hunk.buffer_line_range.clone()];
    }
    hunks.sort_by_key(|h| h.buffer_start_line);
    hunks
}

/// Replace each `Modified` hunk's token detail with the tree differ's spans,
/// clipped to that hunk.
///
/// The two differs answer different questions and this is where they meet. The
/// line pass owns extents and staged marks, so the gutter, staging, and
/// navigation are unchanged. The tree pass owns detail, which is what stops a
/// reindent under a new block from washing every moved line's leading
/// whitespace: tree-sitter sees the statements as unchanged nodes that moved,
/// where the line differ sees every line's prefix rewritten.
///
/// A `Modified` hunk the tree pass finds nothing in takes an empty
/// [`TokenDetail`] rather than `None`. The renderer washes nothing and softens
/// nothing for an empty span list, which is the intended reading of a
/// whitespace-only row: gutter-marked, full strength, wash-free. `None` would
/// instead mean "no detail resolved" and leave the row to the caller's default.
///
/// `Added` and `Deleted` hunks keep `None`, which is what suppresses a
/// whole-line wash on them.
///
/// Spans stay file-absolute, exactly as [`replaced_change_spans`] leaves them.
pub(crate) fn merge_structural_detail(
    hunks: &mut [DiffHunk],
    tree_changes: &[stoat_language::structural_diff::DiffChange],
    buffer_text: &str,
) {
    use stoat_language::structural_diff::{ChangeKind as LangChangeKind, Side};

    let buffer_starts = line_starts(buffer_text);
    for hunk in hunks.iter_mut() {
        if hunk.status != DiffHunkStatus::Modified {
            continue;
        }
        let buffer_bytes =
            line_range_to_byte_range(&buffer_starts, buffer_text.len(), &hunk.buffer_line_range);
        let mut buffer_spans = Vec::new();
        let mut base_spans = Vec::new();
        for change in tree_changes {
            // A relocation is not a content change. The tree differ reports every
            // token of a reindented block as moved, so washing those would mark
            // a run of untouched statements more heavily than the line differ
            // did -- the clutter this pass exists to remove. Whole-hunk moves
            // are a separate status and keep their own cyan wash.
            if change.kind == LangChangeKind::Moved {
                continue;
            }
            let (bounds, out) = match change.side {
                Side::Rhs => (&buffer_bytes, &mut buffer_spans),
                Side::Lhs => (&hunk.base_byte_range, &mut base_spans),
            };
            out.extend(
                effective_ranges(change)
                    .iter()
                    .filter(|range| range.start < bounds.end && bounds.start < range.end)
                    .map(|range| ChangeSpan {
                        byte_range: range.clone(),
                        kind: span_kind(change),
                        move_metadata: change.move_metadata.clone(),
                    }),
            );
        }
        hunk.marked_rows = marked_row_runs(&buffer_spans, |off| {
            line_of(&buffer_starts, off.min(buffer_text.len()))
        });
        hunk.token_detail = Some(Arc::new(TokenDetail {
            buffer_spans,
            base_spans,
        }));
    }
}

/// The byte ranges a change actually marks.
///
/// `refined_spans` when the differ narrowed the change to the characters that
/// differ, else the whole `byte_range`, the same preference
/// [`replaced_change_spans`] makes.
fn effective_ranges(change: &stoat_language::structural_diff::DiffChange) -> &[Range<usize>] {
    match change.refined_spans.is_empty() {
        true => std::slice::from_ref(&change.byte_range),
        false => change.refined_spans.as_slice(),
    }
}

/// How a tree change washes.
///
/// Only the two content kinds reach this. A moved change never becomes a span.
fn span_kind(change: &stoat_language::structural_diff::DiffChange) -> ChangeKind {
    use stoat_language::structural_diff::ChangeKind as LangChangeKind;
    match change.kind {
        LangChangeKind::Replaced => ChangeKind::Replaced,
        LangChangeKind::Novel | LangChangeKind::Moved => ChangeKind::Novel,
    }
}

/// The byte span buffer lines `range` covers.
fn line_range_to_byte_range(
    line_starts: &[usize],
    text_len: usize,
    range: &Range<u32>,
) -> Range<usize> {
    let start = line_starts
        .get(range.start as usize)
        .copied()
        .unwrap_or(text_len);
    let end = line_starts
        .get(range.end as usize)
        .copied()
        .unwrap_or(text_len);
    start..end.max(start)
}
/// The changed sub-ranges of one side of a `Replaced` pair, as
/// [`ChangeKind::Replaced`] [`ChangeSpan`]s.
///
/// Prefers the structural diff's `refined_spans` -- the char ranges that
/// actually differ -- so a one-word edit records only that word. An empty
/// `refined_spans` means the whole token changed, so the whole `byte_range`
/// becomes the single span and a full rewrite still marks completely.
fn replaced_change_spans(change: &stoat_language::structural_diff::DiffChange) -> Vec<ChangeSpan> {
    let ranges = if change.refined_spans.is_empty() {
        std::slice::from_ref(&change.byte_range)
    } else {
        change.refined_spans.as_slice()
    };
    ranges
        .iter()
        .map(|range| ChangeSpan {
            byte_range: range.clone(),
            kind: ChangeKind::Replaced,
            move_metadata: None,
        })
        .collect()
}

/// Map each base line a hunk removed to that hunk's staged state.
///
/// A hunk's [`DiffHunk::base_byte_range`] spans the base content it removed,
/// including the trailing newline, so the covered line count comes from
/// [`str::lines`] rather than the byte-to-line range, which would over-count by
/// one at a newline boundary. Added hunks have an empty base range and map no
/// line.
fn compute_base_staged(hunks: &SumTree<DiffHunk>, base_text: Option<&Arc<String>>) -> BaseStaged {
    let Some(base_text) = base_text else {
        return BTreeMap::new();
    };
    let starts = line_starts(base_text);
    let mut out = BTreeMap::new();
    for hunk in hunks.iter() {
        if hunk.base_byte_range.is_empty() {
            continue;
        }
        let start_line = line_of(&starts, hunk.base_byte_range.start);
        let count = base_text[hunk.base_byte_range.clone()].lines().count() as u32;
        let staged = hunk.staged();
        for line in start_line..start_line + count {
            out.insert(line, staged);
        }
    }
    out
}

/// Distribute every hunk's base change spans across the base lines they cover,
/// keyed by base line with each range line-local within it and tagged with its
/// [`ChangeKind`], so the diff view's left column can wash each span by kind.
///
/// Resolved once by [`DiffMap::from_hunks`] because `hunks` and `base_text` are
/// immutable after construction. Empty when there is no base text.
fn compute_base_change_spans(
    hunks: &SumTree<DiffHunk>,
    base_text: Option<&Arc<String>>,
) -> BaseChangeSpans {
    let Some(base_text) = base_text else {
        return BTreeMap::new();
    };
    let starts = line_starts(base_text);
    let mut out: BaseChangeSpans = BTreeMap::new();
    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.base_spans {
            distribute_change_span(
                &mut out,
                &span.byte_range,
                span.kind.clone(),
                &starts,
                base_text.len(),
            );
        }
    }
    out
}

/// Split an absolute base-text byte `range` into per-line-local ranges, pushing
/// each onto `out` under its base line.
///
/// `line_starts` gives each base line's byte offset, and `text_len` closes the
/// last line. A range spanning several lines contributes one clamped sub-range
/// per line it covers, with the trailing newline excluded.
fn distribute_change_span(
    out: &mut BaseChangeSpans,
    range: &Range<usize>,
    kind: ChangeKind,
    line_starts: &[usize],
    text_len: usize,
) {
    let first = line_starts
        .partition_point(|&start| start <= range.start)
        .saturating_sub(1);
    for line in first..line_starts.len() {
        let line_start = line_starts[line];
        if line_start >= range.end {
            break;
        }
        let line_end = line_starts
            .get(line + 1)
            .map(|&next| next.saturating_sub(1))
            .unwrap_or(text_len);
        let start = range.start.max(line_start);
        let end = range.end.min(line_end);
        if start < end {
            out.entry(line as u32)
                .or_default()
                .push(((start - line_start)..(end - line_start), kind.clone()));
        }
    }
}

fn byte_range_to_line_range(
    line_starts: &[usize],
    text_len: usize,
    byte_range: &Range<usize>,
) -> Range<u32> {
    let start_byte = byte_range.start.min(text_len);
    let end_byte = byte_range.end.min(text_len);
    let start_line = line_of(line_starts, start_byte);
    // For an empty range, return start..start so callers can detect it.
    if start_byte == end_byte {
        return start_line..start_line;
    }
    start_line..(line_of(line_starts, end_byte) + 1)
}

/// Byte offset at the start of each line, line 0 at offset 0. Precomputed once
/// per side so each byte-to-line conversion is a binary search rather than a
/// prefix rescan.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

/// The 0-based line containing `byte`, resolved against a [`line_starts`] table.
///
/// Equals the number of newlines before `byte`, matching a prefix newline
/// count. The table is seeded with 0, so the count is at least one and the
/// subtraction never underflows.
fn line_of(line_starts: &[usize], byte: usize) -> u32 {
    (line_starts.partition_point(|&start| start <= byte) - 1) as u32
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail};
    use crate::{display_map::BlockPlacement, host::DiffStatus};
    use std::sync::Arc;

    #[test]
    fn line_mapping_matches_prefix_newline_count() {
        // The two newlines at bytes 2 and 5 put line starts at 0, 3, and 6.
        let text = "ab\ncd\nef";
        let starts = super::line_starts(text);
        assert_eq!(starts, vec![0, 3, 6]);

        assert_eq!(super::line_of(&starts, 0), 0, "first byte is line 0");
        assert_eq!(
            super::line_of(&starts, 2),
            0,
            "the newline byte stays on line 0"
        );
        assert_eq!(
            super::line_of(&starts, 3),
            1,
            "first byte past a newline is line 1"
        );
        assert_eq!(super::line_of(&starts, 7), 2, "last byte is line 2");
        assert_eq!(
            super::line_of(&starts, 99),
            2,
            "a byte past EOF clamps to the last line"
        );

        let lines = |range| super::byte_range_to_line_range(&starts, text.len(), &range);
        assert_eq!(lines(3..5), 1..2, "a single-line range spans one line");
        assert_eq!(
            lines(6..6),
            2..2,
            "an empty range collapses to start..start"
        );
        assert_eq!(
            lines(0..7),
            0..3,
            "a multi-line range covers start through end inclusive"
        );
    }

    fn added_hunk(line_range: std::ops::Range<u32>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Added,
            unstaged_lines: vec![line_range.clone()],
            marked_rows: Vec::new(),
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    /// Two recomputes of an unchanged file share a base handle, so the question
    /// is answered by the pointers. The fallback still has to answer it for two
    /// bases that hold the same bytes in different allocations, which is what a
    /// pointer test alone would get wrong.
    #[test]
    fn a_shared_base_compares_equal_and_a_copied_one_still_does() {
        let base = Arc::new("alpha\nbeta\n".to_string());
        let with = |base: Arc<String>, range: std::ops::Range<u32>| {
            DiffMap::from_hunks([added_hunk(range)], Some(base))
        };

        let first = with(base.clone(), 3..4);
        assert!(
            first.renders_same_as(&with(base.clone(), 3..4)),
            "the same base and the same hunks render alike",
        );
        assert!(
            first.renders_same_as(&with(Arc::new(String::clone(&base)), 3..4)),
            "and so does a base holding the same bytes somewhere else",
        );

        assert!(
            !first.renders_same_as(&with(Arc::new("gamma\n".to_string()), 3..4)),
            "a different base renders differently however the hunks line up",
        );
        assert!(
            !first.renders_same_as(&with(base, 5..6)),
            "and so do different hunks over the same base",
        );
    }

    /// The rows a hunk was built with go stale the moment the reader types.
    /// Its anchors are what still name the same text afterwards.
    #[test]
    fn anchored_hunks_follow_the_text_when_a_row_is_inserted_above() {
        use crate::buffer::{BufferId, TextBuffer};

        let mut buffer = TextBuffer::with_text(BufferId::new(0), "l0\nl1\nl2\nl3\nl4\n");
        let mut dm = DiffMap::from_hunks([added_hunk(3..4)], None);
        dm.anchor_hunks(&buffer.snapshot);

        let rows_now = |buffer: &TextBuffer| {
            let hunk = dm.hunks().next().expect("one hunk");
            let range = hunk.anchor_range.clone().expect("anchored");
            let rope = &buffer.snapshot.visible_text;
            let row_of = |anchor| {
                rope.offset_to_point(buffer.snapshot.resolve_anchor(&anchor))
                    .row
            };
            row_of(range.start)..row_of(range.end)
        };

        assert_eq!(rows_now(&buffer), 3..4, "the rows it was built with");

        buffer.edit(0..0, "new\n");
        assert_eq!(
            rows_now(&buffer),
            4..5,
            "a row inserted above carries the hunk down with the text",
        );
    }

    /// The diff runs on a background job spawned per buffer version, so its
    /// rows are behind for as long as that takes. The gutter has to answer for
    /// the text on screen in the meantime.
    #[test]
    fn a_gutter_mark_follows_its_text_before_the_next_diff_lands() {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        use std::sync::RwLock;

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "l0\nl1\nl2\nl3\nl4\n",
        )));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let mut dm = DiffMap::from_hunks([added_hunk(3..4)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let marked_row = |dm: &DiffMap| {
            let snapshot = multi.snapshot();
            let live = dm.live_hunks(&snapshot);
            (0..10).find(|&row| live.gutter_mark_for_line(row).is_some())
        };

        assert_eq!(marked_row(&dm), Some(3), "the row the diff marked");

        shared.write().expect("poisoned").edit(0..0, "new\n");
        assert_eq!(
            marked_row(&dm),
            Some(4),
            "and the mark rides the inserted row down with its text",
        );
        assert!(
            dm.gutter_mark_for_line(3).is_some(),
            "while the stored rows still name where the diff ran",
        );
    }

    /// A file with no trailing newline has no row start after its last line. An
    /// exclusive end anchored at a row start therefore lands on the last line
    /// itself, and leaves the hunk covering nothing.
    /// Four callers ask per frame, so the second one through has to answer the
    /// same as the first, and an edit has to be seen rather than served from
    /// what the edit moved.
    #[test]
    fn live_hunks_repeats_its_answer_and_re_resolves_after_an_edit() {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        use std::sync::RwLock;

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "l0\nl1\nl2\nl3\n",
        )));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let mut dm = DiffMap::from_hunks([added_hunk(2..3)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let rows = |dm: &DiffMap, multi: &MultiBuffer| -> Vec<std::ops::Range<u32>> {
            dm.live_hunks(&multi.snapshot())
                .in_range(0..10)
                .map(|(_, rows)| rows)
                .collect()
        };

        let first = rows(&dm, &multi);
        assert_eq!(first, vec![2..3], "the hunk covers the row it was built on");
        assert_eq!(
            rows(&dm, &multi),
            first,
            "and asking again answers the same"
        );

        // A line inserted above pushes the hunk down, which the cached rows
        // predate.
        shared.write().expect("poisoned").edit(0..0, "inserted\n");
        assert_eq!(
            rows(&dm, &multi),
            vec![3..4],
            "the edit moves the hunk rather than being served the old rows",
        );
    }

    #[test]
    fn a_hunk_on_an_unterminated_last_line_keeps_its_gutter_mark() {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        use std::sync::RwLock;

        // The base holds "a\n" and the buffer appended "b" without a newline.
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), "a\nb")));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let mut dm = DiffMap::from_hunks([added_hunk(1..2)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let live = dm.live_hunks(&multi.snapshot());
        assert_eq!(
            live.gutter_mark_for_line(1),
            Some((DiffHunkStatus::Added, false)),
            "the added last line is marked with no newline after it",
        );
        assert_eq!(
            live.gutter_mark_for_line(0),
            None,
            "the unchanged line above"
        );
    }

    /// Reaching an end past an unterminated last line widens any end resting
    /// inside a row. Both ends of a zero-width hunk rest at one point.
    #[test]
    fn anchored_deletion_and_move_seams_stay_empty() {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        use std::sync::RwLock;

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "l0\nl1\nl2\nl3\nl4\nl5\n",
        )));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let moved = DiffHunk {
            status: DiffHunkStatus::Moved,
            unstaged_lines: std::iter::once(5..5).collect(),
            marked_rows: Vec::new(),
            buffer_start_line: 5,
            buffer_line_range: 5..5,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        };
        let mut dm = DiffMap::from_hunks([deleted_hunk(2, 0..10), moved], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let live = dm.live_hunks(&multi.snapshot());
        assert_eq!(
            live.in_range(0..10)
                .map(|(_, rows)| rows)
                .collect::<Vec<_>>(),
            vec![3..3, 5..5],
            "both seams still cover no rows",
        );
        assert_eq!(
            live.gutter_mark_for_line(3),
            Some((DiffHunkStatus::Deleted, false)),
            "and the deletion seam keeps its mark",
        );
        assert_eq!(
            live.gutter_mark_for_line(5),
            Some((DiffHunkStatus::Moved, false)),
            "as does the move seam",
        );
    }

    /// Lines deleted from the end of a file with no trailing newline leave a
    /// seam on a row that does not exist. That is where a clamped start and an
    /// end reaching the end of the text come apart.
    #[test]
    fn a_deletion_seam_past_an_unterminated_last_line_covers_no_rows() {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        use std::sync::RwLock;

        // The base held "a\nb\nc" and the buffer dropped "c". The seam sits at
        // row 2, one past the buffer's last row.
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), "a\nb")));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let mut dm = DiffMap::from_hunks([deleted_hunk(1, 4..6)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let live = dm.live_hunks(&multi.snapshot());
        assert_eq!(
            live.in_range(0..10)
                .map(|(_, rows)| rows)
                .collect::<Vec<_>>(),
            vec![1..1],
            "the seam falls back onto the last row and covers none of it",
        );
        assert_eq!(
            live.gutter_mark_for_line(1),
            Some((DiffHunkStatus::Deleted, false)),
            "and paints the deletion mark there",
        );
    }

    /// The tally is stepped rather than refolded, so pushing has to move the
    /// bucket matching the pushed hunk and leave the other alone.
    #[test]
    fn pushing_a_hunk_steps_the_staged_tally() {
        let staged = DiffHunk {
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
            ..added_hunk(1..2)
        };
        let recount = |dm: &DiffMap| {
            dm.hunks().fold((0, 0), |(s, u), hunk| {
                if hunk.staged() {
                    (s + 1, u)
                } else {
                    (s, u + 1)
                }
            })
        };

        let mut dm = DiffMap::from_hunks([staged.clone()], None);
        assert_eq!(
            dm.staged_counts(),
            (1, 0),
            "the staged hunk it was built with"
        );

        dm.push_hunk(added_hunk(3..4));
        assert_eq!(
            dm.staged_counts(),
            (1, 1),
            "an unstaged push moves only that bucket"
        );
        assert_eq!(
            dm.staged_counts(),
            recount(&dm),
            "and agrees with a full recount"
        );

        dm.push_hunk(DiffHunk {
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
            ..added_hunk(5..6)
        });
        assert_eq!(dm.staged_counts(), (2, 1));
        assert_eq!(dm.staged_counts(), recount(&dm), "and still agrees");
    }

    fn deleted_hunk(after_line: u32, base_byte_range: std::ops::Range<usize>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Deleted,
            unstaged_lines: std::iter::once((after_line + 1)..(after_line + 1)).collect(),
            marked_rows: Vec::new(),
            buffer_start_line: after_line + 1,
            buffer_line_range: (after_line + 1)..(after_line + 1),
            base_byte_range,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn modified_hunk(
        line_range: std::ops::Range<u32>,
        base_byte_range: std::ops::Range<usize>,
    ) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Modified,
            unstaged_lines: vec![line_range.clone()],
            marked_rows: Vec::new(),
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range,
            anchor_range: None,
            token_detail: None,
        }
    }

    #[test]
    fn gutter_mark_reports_status_and_deletion_seam() {
        let mut a = added_hunk(1..3);
        a.unstaged_lines.clear();
        let m = modified_hunk(5..6, 10..14);
        let mut d = deleted_hunk(8, 20..30);
        d.unstaged_lines.clear();

        let dm = DiffMap::from_hunks([a, m, d], None);

        assert_eq!(
            dm.gutter_mark_for_line(1),
            Some((DiffHunkStatus::Added, true)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(2),
            Some((DiffHunkStatus::Added, true)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(3),
            None,
            "a row past the added range is unmarked",
        );
        assert_eq!(
            dm.gutter_mark_for_line(5),
            Some((DiffHunkStatus::Modified, false)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(9),
            Some((DiffHunkStatus::Deleted, true)),
            "the deletion seam anchors on the row below the removed lines",
        );
        assert_eq!(dm.gutter_mark_for_line(0), None);
    }

    #[test]
    fn gutter_mark_reports_the_moved_seam() {
        // A moved-away seam is a Moved hunk with an empty buffer line range
        // anchored at line 3.
        let seam = DiffHunk {
            status: DiffHunkStatus::Moved,
            unstaged_lines: std::iter::once(3..3).collect(),
            marked_rows: Vec::new(),
            buffer_start_line: 3,
            buffer_line_range: 3..3,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        };
        let dm = DiffMap::from_hunks([seam], None);

        assert_eq!(
            dm.gutter_mark_for_line(3),
            Some((DiffHunkStatus::Moved, false)),
            "the moved-away seam anchors a Moved gutter mark",
        );
        assert_eq!(dm.gutter_mark_for_line(2), None);
        assert_eq!(dm.gutter_mark_for_line(4), None);
    }

    #[test]
    fn interleaved_replacements_group_by_pair_id() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        let lhs_text = "alpha\nbeta\ngamma\ndelta\n";
        let rhs_text = "ALPHA\nbeta\nGAMMA\ndelta\n";
        // Changes emitted in interleaved order: Lhs(alpha), Lhs(gamma),
        // Rhs(ALPHA), Rhs(GAMMA). Without pair_ids the old pairing pass
        // would mis-pair Lhs(gamma) with Rhs(ALPHA).
        let changes = vec![
            DiffChange {
                side: Side::Lhs,
                byte_range: 0..5,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..5,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );
        let hunks: Vec<&DiffHunk> = dm.hunks_in_range(0..10);
        let modified_hunks: Vec<&&DiffHunk> = hunks
            .iter()
            .filter(|h| h.status == DiffHunkStatus::Modified)
            .collect();
        assert_eq!(
            modified_hunks.len(),
            2,
            "two paired replacements must produce two Modified hunks: {hunks:?}"
        );
        // Pair 0: ALPHA maps to alpha's byte range.
        let p0 = modified_hunks
            .iter()
            .find(|h| h.buffer_start_line == 0)
            .expect("pair 0 hunk");
        assert_eq!(p0.base_byte_range, 0..5);
        // Pair 1: GAMMA maps to gamma's byte range.
        let p1 = modified_hunks
            .iter()
            .find(|h| h.buffer_start_line == 2)
            .expect("pair 1 hunk");
        assert_eq!(p1.base_byte_range, 11..16);
    }

    #[test]
    fn modified_hunk_carries_refined_token_spans() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        let lhs_text = "let s = \"hello world\";\n";
        let rhs_text = "let s = \"hello brave world\";\n";
        // The buffer inserts "brave " (bytes 15..21) into the string literal.
        // The Rhs change refines to just that word. The Lhs change has no
        // refinement, so its base span falls back to the whole literal.
        let brave = 15..21;
        let changes = vec![
            DiffChange {
                side: Side::Lhs,
                byte_range: 8..21,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 8..27,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: vec![brave.clone()],
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );

        let td = dm
            .token_detail_for_line(0)
            .expect("modified hunk carries token detail");
        assert_eq!(
            td.buffer_spans,
            vec![ChangeSpan {
                byte_range: brave.clone(),
                kind: ChangeKind::Replaced,
                move_metadata: None,
            }],
            "buffer spans narrow to the inserted word"
        );
        assert_eq!(
            td.base_spans,
            vec![ChangeSpan {
                byte_range: 8..21,
                kind: ChangeKind::Replaced,
                move_metadata: None,
            }],
            "base spans fall back to the whole replaced literal"
        );
        assert_eq!(&rhs_text[brave], "brave ");
    }

    #[test]
    fn base_change_spans_split_across_base_lines() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        // A two-line base region replaced wholesale (no refinement) must
        // distribute into one line-local span per base line, newline excluded.
        let lhs_text = "alpha\nbeta\n";
        let rhs_text = "ALPHA\nBETA\n";
        let changes = vec![
            DiffChange {
                side: Side::Lhs,
                byte_range: 0..10,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..10,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );

        let flat: Vec<(u32, usize, usize, ChangeKind)> = dm
            .base_change_spans()
            .iter()
            .flat_map(|(&line, ranges)| {
                ranges
                    .iter()
                    .map(move |(r, kind)| (line, r.start, r.end, kind.clone()))
            })
            .collect();
        assert_eq!(
            flat,
            vec![
                (0, 0, 5, ChangeKind::Replaced),
                (1, 0, 4, ChangeKind::Replaced)
            ],
            "alpha on line 0, beta on line 1"
        );
    }

    #[test]
    fn deletion_anchors_to_rhs_line() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        let lhs_text = "keep\nremove me\nkeep2\n";
        let rhs_text = "keep\nkeep2\n";
        let changes = vec![DiffChange {
            side: Side::Lhs,
            byte_range: 5..15,
            kind: LangChangeKind::Novel,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: Some(1),
            refined_spans: Vec::new(),
        }];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );
        let hunks: Vec<&DiffHunk> = dm.hunks_in_range(0..5);
        let deleted = hunks
            .iter()
            .find(|h| h.status == DiffHunkStatus::Deleted)
            .expect("deleted hunk");
        assert_eq!(
            deleted.buffer_start_line, 1,
            "anchor must override default lhs-line positioning: {deleted:?}"
        );
    }

    #[test]
    fn deletion_without_anchor_falls_back_to_lhs_line() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        let lhs_text = "alpha\nbeta\ngamma\n";
        let rhs_text = "alpha\n";
        let changes = vec![DiffChange {
            side: Side::Lhs,
            byte_range: 6..16,
            kind: LangChangeKind::Novel,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
            refined_spans: Vec::new(),
        }];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );
        let hunks: Vec<&DiffHunk> = dm.hunks_in_range(0..5);
        let deleted = hunks
            .iter()
            .find(|h| h.status == DiffHunkStatus::Deleted)
            .expect("deleted hunk");
        // Falls back to counting newlines before the lhs byte range.
        assert_eq!(deleted.buffer_start_line, 1);
    }

    #[test]
    fn empty_map_returns_unchanged() {
        let dm = DiffMap::default();
        assert_eq!(dm.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(100), DiffStatus::Unchanged);
        assert!(!dm.has_deletion_after(0, true));
        assert!(dm.is_empty());
        assert_eq!(dm.total_deleted_lines(), 0);
        assert!(dm.deleted_blocks(true).is_empty());
    }

    #[test]
    fn single_added_hunk() {
        let dm = DiffMap::from_hunks([added_hunk(5..8)], None);

        assert_eq!(dm.status_for_line(4), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(5), DiffStatus::Added);
        assert_eq!(dm.status_for_line(6), DiffStatus::Added);
        assert_eq!(dm.status_for_line(7), DiffStatus::Added);
        assert_eq!(dm.status_for_line(8), DiffStatus::Unchanged);
        assert!(!dm.has_deletion_after(4, true));
        assert!(dm.deleted_blocks(true).is_empty());
    }

    #[test]
    fn single_deleted_hunk() {
        let base = "deleted line\n";
        let dm = DiffMap::from_hunks([deleted_hunk(2, 0..13)], Some(Arc::new(base.to_string())));

        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
        assert!(dm.has_deletion_after(2, true));
        assert!(!dm.has_deletion_after(1, true));
        assert!(!dm.has_deletion_after(3, true));

        let blocks = dm.deleted_blocks(true);
        assert_eq!(blocks.len(), 1);
        let ctx = crate::display_map::BlockContext {
            block_id: crate::display_map::BlockId::Custom(crate::display_map::CustomBlockId(0)),
            max_width: 80,
            height: blocks[0].height.unwrap_or(0),
            selected: false,
            anchor_row: 0,
            diff_status: None,
            buffer_snapshot: &crate::multi_buffer::MultiBufferSnapshot::empty(),
        };
        let lines = (blocks[0].render)(&ctx);
        assert_eq!(lines[0].to_string(), "deleted line");
        assert_eq!(dm.total_deleted_lines(), 1);
    }

    #[test]
    fn single_modified_hunk() {
        let base = "old content\n";
        let dm = DiffMap::from_hunks(
            [modified_hunk(3..5, 0..12)],
            Some(Arc::new(base.to_string())),
        );

        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(3), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(4), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(5), DiffStatus::Unchanged);

        // One base line against two live rows, so the base row pairs with the
        // first of them and nothing is left to block.
        assert_eq!(dm.deleted_blocks(true).len(), 0);
        assert!(!dm.has_deletion_after(2, true));
        assert!(!dm.has_deletion_after(4, true));
    }

    /// A base longer than the live side pairs what it can and blocks the rest
    /// after the hunk's last live row, so the filler a length difference costs
    /// lands at the end of the hunk rather than above it.
    #[test]
    fn a_longer_base_hunk_blocks_only_its_excess_after_the_live_rows() {
        let base = "a\nb\nc\n";
        let dm = DiffMap::from_hunks(
            [modified_hunk(3..4, 0..6)],
            Some(Arc::new(base.to_string())),
        );

        let blocks = dm.deleted_blocks(true);
        assert_eq!(blocks.len(), 1, "the two base rows past the live one block");
        assert_eq!(
            (blocks[0].placement, blocks[0].height),
            (BlockPlacement::Below(3), Some(2)),
            "and hang off the hunk's last live row",
        );

        assert!(dm.has_deletion_after(3, true), "the block sits below row 3");
        assert!(!dm.has_deletion_after(2, true), "not above the hunk");
    }

    /// The count the diff view maps a viewport top through. A row is
    /// base-present while its hunk's base rows reach it, so an added hunk's
    /// rows all count and a modified hunk's only past its paired prefix.
    #[test]
    fn rows_without_base_counts_added_and_unpaired_modified_rows() {
        let base = "a\n";
        let dm = DiffMap::from_hunks(
            [added_hunk(1..3), modified_hunk(5..8, 0..2)],
            Some(Arc::new(base.to_string())),
        );

        assert_eq!(
            dm.rows_without_base_before(5, true),
            2,
            "the added hunk's rows"
        );
        assert_eq!(
            dm.rows_without_base_before(7, true),
            3,
            "and the modified hunk's second row, its first being paired",
        );
        assert_eq!(dm.rows_without_base_before(9, true), 4, "and its third");
    }

    #[test]
    fn multiple_hunks() {
        let base = "del1\ndel2\n";
        let dm = DiffMap::from_hunks(
            [
                added_hunk(1..3),
                deleted_hunk(4, 0..5),
                modified_hunk(7..9, 5..10),
            ],
            Some(Arc::new(base.to_string())),
        );

        assert_eq!(dm.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(1), DiffStatus::Added);
        assert_eq!(dm.status_for_line(2), DiffStatus::Added);
        assert_eq!(dm.status_for_line(3), DiffStatus::Unchanged);
        assert!(dm.has_deletion_after(4, true));
        assert_eq!(dm.status_for_line(7), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(8), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(9), DiffStatus::Unchanged);

        // The deleted hunk blocks whole; the modified one's single base line
        // pairs with the first of its two live rows.
        assert_eq!(dm.deleted_blocks(true).len(), 1);
    }

    #[test]
    fn hunks_in_range_viewport() {
        let dm = DiffMap::from_hunks(
            [added_hunk(2..4), added_hunk(8..10), added_hunk(15..17)],
            None,
        );

        let visible = dm.hunks_in_range(5..12);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].buffer_line_range, 8..10);

        let all = dm.hunks_in_range(0..20);
        assert_eq!(all.len(), 3);

        let overlap = dm.hunks_in_range(3..9);
        assert_eq!(overlap.len(), 2);
    }

    #[test]
    fn token_detail_for_line_returns_spans() {
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 0..5,
                kind: ChangeKind::Novel,
                move_metadata: None,
            }],
            base_spans: vec![],
        });
        let mut hunk = modified_hunk(3..5, 0..10);
        hunk.token_detail = Some(detail.clone());

        let dm = DiffMap::from_hunks([hunk], Some(Arc::new("old content".to_string())));

        assert!(dm.token_detail_for_line(2).is_none());
        let td = dm.token_detail_for_line(3).unwrap();
        assert_eq!(td.buffer_spans.len(), 1);
        assert_eq!(td.buffer_spans[0].byte_range, 0..5);
        assert!(dm.token_detail_for_line(5).is_none());
    }

    #[test]
    fn token_detail_none_when_not_set() {
        let dm = DiffMap::from_hunks([added_hunk(3..5)], None);
        assert!(dm.token_detail_for_line(3).is_none());
    }

    #[test]
    fn hunk_at_line_zero() {
        let dm = DiffMap::from_hunks([added_hunk(0..2)], None);
        assert_eq!(dm.status_for_line(0), DiffStatus::Added);
        assert_eq!(dm.status_for_line(1), DiffStatus::Added);
        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
    }

    #[test]
    fn deleted_hunk_after_line_zero() {
        let base = "removed\n";
        let dm = DiffMap::from_hunks([deleted_hunk(0, 0..8)], Some(Arc::new(base.to_string())));
        assert!(dm.has_deletion_after(0, true));
        assert!(!dm.has_deletion_after(1, true));
    }

    #[test]
    fn total_deleted_lines_multiline() {
        let base = "line1\nline2\nline3\n";
        let dm = DiffMap::from_hunks([deleted_hunk(0, 0..18)], Some(Arc::new(base.to_string())));
        assert_eq!(dm.total_deleted_lines(), 3);
    }

    #[test]
    fn deleted_hunk_does_not_report_status() {
        let base = "removed\n";
        let dm = DiffMap::from_hunks([deleted_hunk(5, 0..8)], Some(Arc::new(base.to_string())));
        assert_eq!(dm.status_for_line(5), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(6), DiffStatus::Unchanged);
    }

    #[test]
    fn from_structural_changes_addition() {
        // Pure RHS addition: a new line in the buffer that has no
        // counterpart in base.
        let lhs = "alpha\nbeta\n";
        let rhs = "alpha\nbeta\ngamma\n";
        let result = stoat_language::structural_diff::diff(lhs, rhs);
        let dm = DiffMap::from_structural_changes(result, Arc::new(lhs.to_string()), rhs);
        // The added line is on buffer line 2 (zero-indexed).
        assert_eq!(dm.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(1), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(2), DiffStatus::Added);
    }

    #[test]
    fn from_structural_changes_modification() {
        // A single replaced line.
        let lhs = "alpha\nbeta\ngamma\n";
        let rhs = "alpha\nBETA\ngamma\n";
        let result = stoat_language::structural_diff::diff(lhs, rhs);
        let dm = DiffMap::from_structural_changes(result, Arc::new(lhs.to_string()), rhs);
        assert_eq!(dm.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(1), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
    }

    #[test]
    fn from_structural_changes_identical_inputs() {
        let txt = "one\ntwo\nthree\n";
        let result = stoat_language::structural_diff::diff(txt, txt);
        let dm = DiffMap::from_structural_changes(result, Arc::new(txt.to_string()), txt);
        assert!(dm.is_empty());
    }

    #[test]
    fn from_structural_changes_leaves_hunks_unstaged() {
        let lhs = "a\nb\nc\n";
        let rhs = "a\nB\nc\n";
        let result = stoat_language::structural_diff::diff(lhs, rhs);
        let dm = DiffMap::from_structural_changes(result, Arc::new(lhs.to_string()), rhs);
        assert!(
            dm.hunks_in_range(0..u32::MAX).iter().all(|h| !h.staged()),
            "index-unaware construction reads as entirely unstaged"
        );
    }

    /// Marks for every row of `hunks`' combined range, `None` where unmarked.
    fn marks_over(
        hunks: Vec<DiffHunk>,
        rows: std::ops::Range<u32>,
        text: &str,
    ) -> Vec<Option<DiffHunkStatus>> {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        let map = DiffMap::from_hunks(hunks, Some(Arc::new(text.to_string())));
        let tb = TextBuffer::with_text(BufferId::new(0), text);
        let multi = MultiBuffer::singleton(BufferId::new(0), Arc::new(std::sync::RwLock::new(tb)));
        let snapshot = multi.snapshot();
        let live = map.live_hunks(&snapshot);
        rows.map(|row| live.gutter_mark_for_line(row).map(|(status, _)| status))
            .collect()
    }

    /// A hunk over `rows`, refined to `marked` when non-empty.
    fn refined_hunk(rows: std::ops::Range<u32>, marked: Vec<std::ops::Range<u32>>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: rows.start,
            buffer_line_range: rows.clone(),
            base_byte_range: 0..1,
            anchor_range: None,
            token_detail: None,
            unstaged_lines: std::iter::once(rows).collect(),
            marked_rows: marked,
        }
    }

    /// The stops a map offers over `text`.
    fn stops_of(hunks: Vec<DiffHunk>, text: &str) -> Vec<std::ops::Range<u32>> {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        let map = DiffMap::from_hunks(hunks, Some(Arc::new(text.to_string())));
        let tb = TextBuffer::with_text(BufferId::new(0), text);
        let multi = MultiBuffer::singleton(BufferId::new(0), Arc::new(std::sync::RwLock::new(tb)));
        map.live_hunks(&multi.snapshot()).change_stops()
    }

    /// Navigation stops on what changed. One refined hunk over a block offers a
    /// stop per run, so a walk crosses the changed rows rather than stepping
    /// over the whole block in one press.
    #[test]
    fn a_refined_hunk_offers_a_stop_per_marked_run() {
        let text = "0\n1\n2\n3\n4\n5\n";
        assert_eq!(
            stops_of(vec![refined_hunk(0..6, vec![1..2, 4..5])], text),
            [1..2, 4..5],
            "each run is its own stop"
        );
    }

    /// A hunk the tree pass never narrowed keeps offering itself whole, which
    /// is the stop it has always been.
    #[test]
    fn an_unrefined_hunk_offers_its_whole_range_as_one_stop() {
        let text = "0\n1\n2\n3\n";
        assert_eq!(
            stops_of(vec![refined_hunk(1..3, Vec::new())], text),
            std::iter::once(1..3).collect::<Vec<_>>(),
            "the whole range is one stop"
        );
    }

    /// Stops come back in document order however the hunks and runs arrive, so
    /// a walk never doubles back.
    #[test]
    fn stops_arrive_in_document_order() {
        let text = "0\n1\n2\n3\n4\n5\n6\n7\n";
        let hunks = vec![
            refined_hunk(0..3, vec![2..3, 0..1]),
            refined_hunk(4..7, std::iter::once(5..6).collect()),
        ];
        assert_eq!(
            stops_of(hunks, text),
            [0..1, 2..3, 5..6],
            "runs from both hunks interleave in row order"
        );
    }
    /// The whole point: a hunk whose real change is two tokens marks two rows,
    /// not the hundred its extents cover.
    #[test]
    fn a_refined_hunk_marks_only_the_rows_its_spans_touch() {
        let text = "0\n1\n2\n3\n4\n5\n";
        let hunk = refined_hunk(0..6, vec![1..2, 4..5]);

        assert_eq!(
            marks_over(vec![hunk], 0..6, text),
            [
                None,
                Some(DiffHunkStatus::Modified),
                None,
                None,
                Some(DiffHunkStatus::Modified),
                None,
            ],
            "only the marked runs paint"
        );
    }

    /// A hunk the tree pass never narrowed keeps marking whole, so nothing that
    /// gets no structural detail loses its gutter.
    #[test]
    fn an_unrefined_hunk_still_marks_its_whole_range() {
        let text = "0\n1\n2\n3\n";
        let hunk = refined_hunk(1..3, Vec::new());

        assert_eq!(
            marks_over(vec![hunk], 0..4, text),
            [
                None,
                Some(DiffHunkStatus::Modified),
                Some(DiffHunkStatus::Modified),
                None,
            ],
            "every row of the range marks"
        );
    }

    /// An added run never gets structural detail, so it marks whole the way it
    /// always did.
    #[test]
    fn an_added_hunk_marks_its_whole_range() {
        let text = "0\n1\n2\n3\n";
        let hunk = DiffHunk {
            status: DiffHunkStatus::Added,
            buffer_start_line: 1,
            buffer_line_range: 1..3,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
            unstaged_lines: std::iter::once(1..3).collect(),
            marked_rows: Vec::new(),
        };

        assert_eq!(
            marks_over(vec![hunk], 0..4, text),
            [
                None,
                Some(DiffHunkStatus::Added),
                Some(DiffHunkStatus::Added),
                None,
            ],
            "an addition marks every row it added"
        );
    }
    /// The two-phase contract rests on this. The sync open installs a
    /// line-only map and the settle installs the refined one, which differ in
    /// nothing but their spans, so a comparison blind to detail would skip the
    /// install and the refinement would never reach the screen.
    #[test]
    fn a_detail_only_difference_is_not_the_same_render() {
        let hunk = |spans: Vec<ChangeSpan>| DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: 1,
            buffer_line_range: 1..2,
            base_byte_range: 2..4,
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: spans,
                base_spans: Vec::new(),
            })),
            unstaged_lines: std::iter::once(1..2).collect(),
            marked_rows: Vec::new(),
        };
        let base = Arc::new("a\nb\n".to_string());
        let line_pass = DiffMap::from_hunks(
            [hunk(vec![ChangeSpan {
                byte_range: 2..3,
                kind: ChangeKind::Replaced,
                move_metadata: None,
            }])],
            Some(base.clone()),
        );
        let tree_pass = DiffMap::from_hunks([hunk(Vec::new())], Some(base));

        assert!(
            !line_pass.renders_same_as(&tree_pass),
            "a map whose only difference is its spans still has to install"
        );
    }
    #[test]
    fn from_structural_changes_staged_marks_by_index_overlap() {
        // HEAD a/b/c/d; buffer changes line 1 (B) and line 3 (D). The index
        // holds only the line-1 change, so index-vs-buffer differs on line 3.
        let base = "a\nb\nc\nd\n";
        let index = "a\nB\nc\nd\n";
        let buffer = "a\nB\nc\nD\n";
        let index_changed: Vec<std::ops::Range<u32>> = DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(index, buffer),
            Arc::new(index.to_string()),
            buffer,
        )
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| h.buffer_line_range.clone())
        .collect();
        let result = stoat_language::structural_diff::diff(base, buffer);
        let dm = DiffMap::from_structural_changes_staged(
            result,
            Arc::new(base.to_string()),
            buffer,
            &index_changed,
        );
        let flags: Vec<(u32, bool)> = dm
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|h| (h.buffer_start_line, h.staged()))
            .collect();
        assert_eq!(
            flags,
            vec![(1, true), (3, false)],
            "line-1 change staged, line-3 change unstaged"
        );
    }

    /// With nothing staged the index holds HEAD's bytes, so the diff recompute
    /// reads the changed-line set off the hunks it already has rather than
    /// diffing the file a second time. If those two disagreed, every hunk's
    /// staged flag would come out wrong.
    #[test]
    fn an_index_at_head_yields_the_lines_a_second_diff_would() {
        let base = "a\nb\nc\nd\n";
        let buffer = "a\nB\nc\nD\n";

        let second_diff: Vec<std::ops::Range<u32>> = DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(base, buffer),
            Arc::new(base.to_string()),
            buffer,
        )
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| h.buffer_line_range.clone())
        .collect();

        let result = stoat_language::structural_diff::diff(base, buffer);
        let own_hunks: Vec<std::ops::Range<u32>> =
            super::changes_to_hunks(&result.changes, base, buffer)
                .into_iter()
                .map(|h| h.buffer_line_range)
                .collect();
        assert_eq!(
            own_hunks, second_diff,
            "the hunks already in hand name the same lines the index diff would",
        );

        let dm = DiffMap::from_structural_changes_staged(
            result,
            Arc::new(base.to_string()),
            buffer,
            &own_hunks,
        );
        assert!(
            dm.hunks_in_range(0..u32::MAX).iter().all(|h| !h.staged()),
            "an index sitting at HEAD leaves every hunk unstaged",
        );
    }

    #[test]
    fn from_structural_changes_staged_marks_lines_within_a_hunk() {
        // HEAD is a/b/c. The buffer rewrites both b and c, but the index holds
        // only the line-1 rewrite, so index-vs-buffer still differs on line 2.
        let base = "a\nb\nc\n";
        let buffer = "a\nB\nC\n";
        let index_changed = std::iter::once(2..3).collect::<Vec<_>>();
        let result = stoat_language::structural_diff::diff(base, buffer);
        let dm = DiffMap::from_structural_changes_staged(
            result,
            Arc::new(base.to_string()),
            buffer,
            &index_changed,
        );
        assert_eq!(
            dm.staged_for_line(1),
            Some(true),
            "line 1 matches the index, so it reads staged"
        );
        assert_eq!(
            dm.staged_for_line(2),
            Some(false),
            "line 2 still differs from the index, so it reads unstaged"
        );
    }

    #[test]
    fn partially_staged_hunk_reads_per_line() {
        let mut hunk = modified_hunk(1..3, 10..14);
        hunk.unstaged_lines = std::iter::once(2..3).collect();
        let dm = DiffMap::from_hunks([hunk], None);
        assert_eq!(
            dm.gutter_mark_for_line(1),
            Some((DiffHunkStatus::Modified, true)),
            "row one is staged within the hunk"
        );
        assert_eq!(
            dm.gutter_mark_for_line(2),
            Some((DiffHunkStatus::Modified, false)),
            "row two is still unstaged"
        );
        assert_eq!(dm.staged_for_line(1), Some(true));
        assert_eq!(dm.staged_for_line(2), Some(false));
        assert_eq!(
            dm.staged_counts(),
            (0, 1),
            "a partially staged hunk counts as unstaged"
        );
    }

    #[test]
    fn ranges_overlap_treats_empty_as_a_point() {
        use super::ranges_overlap;
        assert!(ranges_overlap(&(1..3), &(2..5)), "standard overlap");
        assert!(
            !ranges_overlap(&(1..3), &(3..5)),
            "half-open, touching does not overlap"
        );
        assert!(
            ranges_overlap(&(2..2), &(1..5)),
            "empty point inside a range"
        );
        assert!(
            ranges_overlap(&(3..3), &(3..3)),
            "coincident deletion points"
        );
        assert!(
            !ranges_overlap(&(2..2), &(3..5)),
            "empty point outside a range"
        );
    }

    #[test]
    fn moved_hunk_round_trips_with_metadata() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, MoveMetadata, MoveSource, Side,
        };

        // Fabricate a minimal DiffResult with a Moved pair so the
        // hunk conversion does not depend on the full tree-sitter
        // pipeline. One LHS Moved DiffChange and one RHS Moved
        // DiffChange share the same Arc<MoveMetadata>.
        let rhs_text = "fn b() { call(x); }\nfn a() { work(); }\n";
        let lhs_text = "fn a() { work(); }\nfn b() { call(x); }\n";

        let lhs_source = MoveSource {
            buffer: None,
            side: Side::Rhs,
            byte_range: 0..18,
            line_range: 0..1,
        };
        let rhs_source = MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 20..39,
            line_range: 1..2,
        };
        let lhs_meta = Arc::new(MoveMetadata {
            sources: vec![lhs_source.clone()],
        });
        let rhs_meta = Arc::new(MoveMetadata {
            sources: vec![rhs_source.clone()],
        });

        let changes = vec![
            DiffChange {
                side: Side::Lhs,
                byte_range: 20..39,
                kind: LangChangeKind::Moved,
                move_metadata: Some(lhs_meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(rhs_meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
        ];
        let result = DiffResult {
            changes,
            fell_back_to_line_diff: false,
        };
        let dm = DiffMap::from_structural_changes(result, Arc::new(lhs_text.to_string()), rhs_text);

        let hunks: Vec<&DiffHunk> = dm.hunks_in_range(0..10);
        assert!(
            hunks.iter().any(|h| h.status == DiffHunkStatus::Moved),
            "must emit at least one Moved hunk; got {hunks:?}"
        );

        let moved = hunks
            .iter()
            .find(|h| h.status == DiffHunkStatus::Moved)
            .expect("moved hunk");
        let detail = moved.token_detail.as_ref().expect("token_detail set");
        // RHS move records emit at least one buffer_span with Moved kind
        // and the metadata Arc.
        assert_eq!(detail.buffer_spans.len(), 1);
        let span = &detail.buffer_spans[0];
        assert_eq!(span.kind, ChangeKind::Moved);
        let span_meta = span
            .move_metadata
            .as_ref()
            .expect("span must carry metadata");
        assert!(Arc::ptr_eq(span_meta, &rhs_meta));
        assert_eq!(span_meta.sources[0].byte_range, 20..39);
    }

    #[test]
    fn mixed_move_and_novel_changes_produce_distinct_hunks() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, MoveMetadata, MoveSource, Side,
        };
        // One Moved pair and one Novel-only RHS addition: the
        // converter must emit both a Moved hunk and an Added hunk.
        let lhs_text = "fn a() { work(); }\n";
        let rhs_text = "fn a() { work(); }\nfn new() {}\nfn a2() { work(); }\n";
        let meta = Arc::new(MoveMetadata {
            sources: vec![MoveSource {
                buffer: None,
                side: Side::Lhs,
                byte_range: 0..18,
                line_range: 0..1,
            }],
        });
        let changes = vec![
            DiffChange {
                side: Side::Rhs,
                byte_range: 19..31,
                kind: LangChangeKind::Novel,
                move_metadata: None,
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 32..51,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: Vec::new(),
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );
        let statuses: Vec<DiffHunkStatus> =
            dm.hunks_in_range(0..20).iter().map(|h| h.status).collect();
        assert!(
            statuses.contains(&DiffHunkStatus::Moved),
            "must have Moved hunk"
        );
        assert!(
            statuses.contains(&DiffHunkStatus::Added)
                || statuses.contains(&DiffHunkStatus::Modified),
            "must have a non-Moved hunk too; got {statuses:?}"
        );
    }
}
