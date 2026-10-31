use crate::{
    buffer::TextBufferSnapshot,
    display_map::{
        highlights::HighlightStyle, BlockPlacement, BlockProperties, BlockStyle, WrapGeometry,
    },
    host::DiffStatus,
    multi_buffer::MultiBufferSnapshot,
};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ops::Range,
    str::Lines,
    sync::{
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
};
use stoat_text::{
    Anchor, Bias, ContextLessSummary, Dimension, Item, Point, Rope, SeekTarget, SumTree,
};

static DIFF_MAP_VERSION_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// One hunk's entry in [`DiffMap::deleted_block_signature`], every input its
/// blocks are built from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DeletedBlockKey {
    status: DiffHunkStatus,
    buffer_start: u32,
    buffer_end: u32,
    base_bytes: Range<usize>,
    /// The geometry the hunk's base lines broke under.
    wrap: Option<WrapGeometry>,
    /// Rows each index-paired live line takes. In the two-column layout a
    /// paired base line's overflow block starts past them, so they stay empty
    /// without a geometry. In the unified layout they say which index-paired
    /// rows a fold hides, so they fill under any geometry.
    live_rows: Vec<u32>,
}

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

/// How a change span sits in unstructured text.
///
/// A string, a comment, or a line of a file with no grammar has no token
/// boundary inside it, so nothing in the text shows where an edit starts and
/// ends. The diff view marks a span there by how much of its text the edit
/// rewrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProseChange {
    /// The span sits in code, where token boundaries frame the edit.
    None,
    /// The span covers a string, a comment, or a line run that changed as a
    /// whole, or one with no counterpart on the other side.
    ///
    /// Every letter and digit of the text changed. A comment marker, a quote,
    /// or a separator that survived does not make the edit partial.
    Whole,
    /// The span covers the changed chars inside a string, a comment, or a line
    /// run that kept some of its letters or digits.
    Part,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSpan {
    pub byte_range: Range<usize>,
    pub kind: ChangeKind,
    pub move_metadata: Option<Arc<stoat_language::structural_diff::MoveMetadata>>,
    /// Whether the span sits inside a string, a comment, or a file with no
    /// grammar, and how much of that text the edit rewrote.
    ///
    /// The diff view marks a prose span more heavily, having nothing else to
    /// lead the eye to it.
    pub prose: ProseChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDetail {
    pub buffer_spans: Vec<ChangeSpan>,
    pub base_spans: Vec<ChangeSpan>,
    /// Whether the tree pass wrote this detail. False for the line pass's char
    /// refinement, which a hunk keeps until the tree pass reaches it.
    ///
    /// Only the merge of the tree pass into the line pass's hunks writes true.
    /// A hunk folded straight from a tree diff's changes reads false.
    pub tree: bool,
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
/// `blank` answers whether a buffer row holds only whitespace.
///
/// Runs come back sorted and merged, with adjacent runs joined, so a caller
/// can binary-search them and a full-hunk refinement collapses to one run.
/// Runs that only whitespace-only rows separate join too. A blank line between
/// two changed statements holds no token for the tree differ to mark, but it
/// sits inside one change, and a gap in the runs there shows that change as two.
fn marked_row_runs(
    spans: &[ChangeSpan],
    line_of: impl Fn(usize) -> u32,
    blank: impl Fn(u32) -> bool,
) -> Vec<Range<u32>> {
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
            Some(last) if run.start <= last.end || (last.end..run.start).all(&blank) => {
                last.end = last.end.max(run.end);
            },
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
    /// The runs are sorted, and no two touch or sit apart by whitespace-only
    /// rows alone, as [`marked_row_runs`] returns them. A blank row between two
    /// changed rows lies inside the joined run, so it is marked and staged with
    /// them. [`LiveHunks::hunk_stops`] tiles the hunk in that order.
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

    /// Whether the tree pass wrote this hunk's detail.
    ///
    /// False for a hunk in a section the tree search gave up on, and for every
    /// hunk until the tree pass lands.
    pub(crate) fn tree_reached(&self) -> bool {
        self.token_detail.as_ref().is_some_and(|detail| detail.tree)
    }

    /// How the tree pass reads stored row `stored_row` of this hunk.
    ///
    /// A `Modified` hunk the tree pass reached marks the rows its runs name.
    /// An addition, a deletion and a move it reached mark every row, since each
    /// of their rows changed whole.
    fn tree_mark(&self, stored_row: u32) -> TreeMark {
        let marked = match self.status {
            DiffHunkStatus::Modified => {
                self.marked_rows.iter().any(|run| run.contains(&stored_row))
            },
            DiffHunkStatus::Added | DiffHunkStatus::Deleted | DiffHunkStatus::Moved => true,
        };
        match (self.tree_reached(), marked) {
            (false, _) => TreeMark::Unrefined,
            (true, true) => TreeMark::Marked,
            (true, false) => TreeMark::Unmarked,
        }
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
///
/// Two hunks that only whitespace-only rows separate are one change. They make
/// one stop, take one position, and mark the rows between them. The hunks
/// themselves stay as the differ made them, because staging and the changes
/// view build from them.
pub struct LiveHunks<'a> {
    hunks: Vec<LiveHunk<'a>>,
    /// The map's staged marks as the buffer rows they paint on now, sorted by
    /// row, each with its mark's status.
    staged: Vec<(Range<u32>, DiffHunkStatus)>,
    /// What the hunks' staged state means in the map.
    kind: MarkKind,
}

/// One hunk as it sits in the buffer now.
struct LiveHunk<'a> {
    hunk: &'a DiffHunk,
    rows: Range<u32>,
    /// Whether only whitespace-only rows separate this hunk's marked rows from
    /// the previous hunk's, which makes the two one change.
    joined_above: bool,
    /// The 0-based index of the change this hunk belongs to.
    change: usize,
}

impl<'a> LiveHunks<'a> {
    /// The diff mark to paint in the gutter for buffer `line`, or `None` when
    /// no hunk touches it. The row-level counterpart of
    /// [`DiffMap::gutter_mark_for_line`], reading live rows rather than stored
    /// ones.
    ///
    /// Every row a hunk covers answers the hunk's line-pass status, with the
    /// [`TreeMark`] the tree pass gives the row. So a reindent whose real
    /// change is two tokens marks two rows for the tree pass. A row no hunk
    /// marks answers the staged mark on it, if any, which reads `Unrefined`.
    ///
    /// A row in the gap between two hunks that only whitespace-only rows
    /// separate takes the mark and the staged flag of the hunk above it, so the
    /// two read as one change. The row lies in no hunk, so the tree pass never
    /// marks it, and it reads `Unmarked`.
    ///
    /// The map's [`MarkKind`] decides whether the state is a git state or an
    /// approval state.
    pub fn gutter_mark_for_line(&self, line: u32) -> Option<GutterMark> {
        self.hunk_mark_for_line(line)
            .or_else(|| staged_row_mark(&self.staged, line))
            .map(|(status, tree, marked)| GutterMark {
                status,
                tree,
                state: MarkState::of(self.kind, marked),
            })
    }

    /// The mark a hunk paints at buffer `line`, before the staged marks.
    fn hunk_mark_for_line(&self, line: u32) -> Option<(DiffHunkStatus, TreeMark, bool)> {
        let index = self
            .hunks
            .partition_point(|live| live.rows.start <= line)
            .checked_sub(1)?;
        let live = &self.hunks[index];
        let hunk = live.hunk;
        // A row past this hunk's marked rows is in the gap to the next hunk,
        // which marks with this hunk when the next one joins it.
        let gap_below = || {
            self.hunks
                .get(index + 1)
                .filter(|next| next.joined_above)
                .map(|_| (hunk.status, TreeMark::Unmarked, hunk.staged()))
        };

        if live.rows.contains(&line) {
            // The staged rows and the marked runs are both recorded in the
            // hunk's own coordinates, so the live row steps back by however far
            // the hunk has moved before either is read.
            let stored = (line + hunk.buffer_line_range.start).saturating_sub(live.rows.start);
            if hunk.refined() && !hunk.marked_rows.iter().any(|run| run.contains(&stored)) {
                let marked = marked_span(hunk, &live.rows);
                if line >= marked.end
                    && let Some(gap) = gap_below()
                {
                    return Some(gap);
                }
                // The first hunk never joins, so a joined one has a hunk above.
                if line < marked.start && live.joined_above {
                    let above = self.hunks[index - 1].hunk;
                    return Some((above.status, TreeMark::Unmarked, above.staged()));
                }
            }
            return Some((
                hunk.status,
                hunk.tree_mark(stored),
                hunk.line_staged(stored),
            ));
        }
        let seam = hunk.buffer_line_range.start;
        if hunk.status == DiffHunkStatus::Deleted && live.rows.start == line {
            return Some((DiffHunkStatus::Deleted, hunk.tree_mark(seam), hunk.staged()));
        }
        if hunk.status == DiffHunkStatus::Moved && live.rows.is_empty() && live.rows.start == line {
            return Some((DiffHunkStatus::Moved, hunk.tree_mark(seam), hunk.staged()));
        }
        gap_below()
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

    /// The row ranges the hunks offer change navigation, in document order.
    ///
    /// A refined hunk contributes one range per marked run, so `n` inside a
    /// hundred-line reindent stops once per change rather than stepping over
    /// the whole block in one press. A hunk the tree pass never narrowed
    /// contributes its full live rows.
    ///
    /// A refined hunk's ranges tile its live rows. Each starts at its run's
    /// first row and reaches the next run, and the first starts at the hunk's
    /// first row. A paired brace or a blank row at the hunk's edge lies in no
    /// run, and staging a stop stages its rows, so a row outside every stop
    /// stays unstaged after every stop the walk offers is staged. A gap joins
    /// the run above it, since a reindented or blank aftermath belongs to the
    /// change above. A blank row between two runs lies inside the joined run,
    /// so it is marked and staged with them, and the changes around it are one
    /// stop.
    ///
    /// A hunk that only whitespace-only rows separate from the hunk above it
    /// is part of that change. Its first stop extends the stop above, so the
    /// walk crosses both hunks in one press.
    ///
    /// A zero-width `Deleted` or `Moved` seam keeps its empty range. That is
    /// what a caller turns into a single-cell landing at the row the walk gives
    /// a seam.
    pub fn hunk_stops(&self) -> Vec<Range<u32>> {
        let mut stops: Vec<Range<u32>> = Vec::with_capacity(self.hunks.len());
        for live in &self.hunks {
            let mut join = live.joined_above;
            let mut push = |stop: Range<u32>| {
                match stops.last_mut() {
                    Some(above) if join => above.end = above.end.max(stop.end),
                    _ => stops.push(stop),
                }
                join = false;
            };
            match live.hunk.refined() {
                // The runs are the hunk's own coordinates, so they shift with
                // it the same way its live rows did.
                true => {
                    let shift = live.rows.start as i64 - live.hunk.buffer_line_range.start as i64;
                    let mut stop_start = live.rows.start;
                    for run in live.hunk.marked_rows.iter().skip(1) {
                        let run_start = (run.start as i64 + shift).max(0) as u32;
                        // An edit inside the hunk leaves the stored runs stale.
                        // A run that starts at or past the live end opens no
                        // stop, so every stop keeps at least one row.
                        if run_start > stop_start && run_start < live.rows.end {
                            push(stop_start..run_start);
                            stop_start = run_start;
                        }
                    }
                    push(stop_start..live.rows.end);
                },
                false => push(live.rows.clone()),
            }
        }
        stops
    }

    /// The row ranges change navigation stops on outside the diff view, in
    /// document order.
    ///
    /// The stops of [`Self::hunk_stops`] and the rows of each staged mark. Under
    /// the index base the two together are every change since HEAD, which is
    /// what a plain pane's gutter marks. The diff view walks
    /// [`Self::hunk_stops`] alone, because a staged mark is no difference from
    /// the base the view compares against.
    pub fn change_stops(&self) -> Vec<Range<u32>> {
        let mut stops = self.hunk_stops();
        stops.extend(self.staged.iter().map(|(rows, _)| rows.clone()));
        stops.sort_by_key(|run| (run.start, run.end));
        stops
    }

    /// One-based position of the change at buffer `row` among the changes, or
    /// `None` when `row` sits before the first one.
    ///
    /// Hunks that only whitespace-only rows separate are one change, the one
    /// the walk crosses in one press, so they share a position.
    ///
    /// A row between two changes answers the one before it. A jump lands the
    /// cursor inside a hunk, so the walk stands on the previous change until it
    /// reaches the next one. A position that blanks out between changes
    /// flickers as the reader moves.
    ///
    /// A zero-width hunk counts from the live row above its seam, where the
    /// diff view's walk lands it, so a seam past the last row counts from the
    /// last row.
    pub fn hunk_index_at(&self, row: u32) -> Option<usize> {
        let hunks = self.hunks.partition_point(|live| {
            let landing = match live.rows.is_empty() {
                true => live.rows.start.saturating_sub(1),
                false => live.rows.start,
            };
            landing <= row
        });
        (hunks > 0).then(|| self.hunks[hunks - 1].change + 1)
    }

    /// How many changes the hunks make, where hunks that only whitespace-only
    /// rows separate count as one.
    ///
    /// The denominator of the diff view's position. It agrees with
    /// [`Self::hunk_index_at`] and with the stops of [`Self::hunk_stops`].
    pub fn change_count(&self) -> usize {
        self.hunks.last().map_or(0, |live| live.change + 1)
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
/// within its line and tagged with its [`ChangeKind`] and its
/// [`ProseChange`]. The diff view's left column marks each span by its kind
/// and by how it sits in unstructured text.
pub(crate) type BaseChangeSpans = BTreeMap<u32, Vec<(Range<usize>, ChangeKind, ProseChange)>>;

/// What the diff view's left column knows of a base line a hunk removed or
/// replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BaseLineMark {
    /// The hunk's git-index staged state, per [`DiffHunk::staged`].
    staged: bool,
    /// Whether the hunk names the line's changed chars, which leaves the chars
    /// outside its spans as unchanged text.
    refined: bool,
    /// The tree pass's reading of the hunk's first buffer row, which a base
    /// line takes as its own.
    tree: TreeMark,
}

type BaseStaged = BTreeMap<u32, BaseLineMark>;

/// A change the base text holds over the text behind it, in base-line
/// coordinates.
///
/// The working tree's own diff has these when its base is the index and the
/// text behind it is HEAD, which makes each one a staged change. `base_lines`
/// is empty for a deletion and sits at the base line after the removed text,
/// the way a [`DiffHunkStatus::Deleted`] hunk's `buffer_line_range` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagedMark {
    pub(crate) status: DiffHunkStatus,
    pub(crate) base_lines: Range<u32>,
}

/// The meaning of the staged state in one diff map.
///
/// The staged state is one flag per row, whatever produced it. The gutter, the
/// diff view, the tally, and the changes view all read that flag. A range
/// review sets it from its approvals, so the approvals reach each of those
/// views. This names which source set it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkKind {
    /// The index, or the commit under review, holds the change.
    #[default]
    Staged,
    /// A range review approved the rows.
    Approved,
}

/// The staged state of one row, as a state bar paints it.
///
/// The map's [`MarkKind`] and the row's staged flag pick one of four states. A
/// painter then picks one color per state and never reads the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkState {
    /// The index, or the commit under review, holds the row's change.
    Staged,
    /// The row's change is not in the index or the commit under review.
    Unstaged,
    /// A range review approved the row.
    Approved,
    /// A range review did not approve the row.
    Unapproved,
}

impl MarkState {
    /// The state of a row in a map of `kind`, staged when `marked`.
    pub(crate) fn of(kind: MarkKind, marked: bool) -> Self {
        match (kind, marked) {
            (MarkKind::Staged, true) => Self::Staged,
            (MarkKind::Staged, false) => Self::Unstaged,
            (MarkKind::Approved, true) => Self::Approved,
            (MarkKind::Approved, false) => Self::Unapproved,
        }
    }
}

/// How the tree pass reads one row the line pass changed.
///
/// The line pass marks every row it changed, and the tree pass then narrows a
/// modified hunk to the rows whose tokens changed. The two answers differ on a
/// row the line pass changed and the tree pass did not, such as a reindented
/// line, so a [`GutterMark`] carries both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TreeMark {
    /// A tree span touches the row, or the hunk is an addition, a deletion or
    /// a move the tree pass reached.
    Marked,
    /// The tree pass reached the hunk and found no change on the row.
    Unmarked,
    /// The tree pass never reached the hunk, or the row comes from a staged
    /// mark, which only a line diff produces.
    Unrefined,
}

/// The line pass's change kind, the tree pass's reading, and the staged state
/// a gutter paints for one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GutterMark {
    pub status: DiffHunkStatus,
    pub tree: TreeMark,
    pub state: MarkState,
}

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
    /// The [`BaseLineMark`] of each base line a hunk removed or replaced, for
    /// the diff view's removed (left-column) rows. Resolved once at
    /// construction alongside [`Self::base_changes`]. Added hunks contribute
    /// no base line.
    base_staged: Arc<BaseStaged>,
    /// The changes the base text holds over the text behind it, sorted by
    /// `base_lines`. Empty unless [`Self::set_staged_marks`] set them.
    pub(crate) staged_marks: Arc<Vec<StagedMark>>,
    /// [`Self::staged_marks`] as the stored buffer rows they paint on, each
    /// with its mark's status, for the stored-coordinate lookups.
    staged_rows: Arc<Vec<(Range<u32>, DiffHunkStatus)>>,
    /// Hunks counted by staged state as `(staged, unstaged)`, with each staged
    /// mark counted as a staged hunk.
    ///
    /// Kept rather than folded on demand because the status bar asks every
    /// frame while the answer moves only when the diff is rebuilt.
    ///
    /// [`Self::from_hunks`] is the only path that builds the tree outside
    /// tests, since a staging change re-runs the diff rather than editing the
    /// hunks in place. Nothing else takes the tree mutably, so the count stays
    /// in step with it.
    staged_tally: (usize, usize),
    /// What the hunks' staged state means in this map.
    mark_kind: MarkKind,
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
    /// Whether each hunk joins the one above it, in answer order.
    joined: Vec<bool>,
    /// The staged marks placed against `rows`.
    staged: Vec<(Range<u32>, DiffHunkStatus)>,
}

impl LiveRowsCache {
    /// The answer [`DiffMap::live_hunks`] gives for this resolve, where `hunk`
    /// names each hunk by its index in tree order.
    ///
    /// `kind` comes from the map on each call. Clones of one map share this
    /// cache, and [`DiffMap::with_mark_kind`] sets the kind on one clone alone.
    fn answer<'a>(&self, hunk: impl Fn(usize) -> &'a DiffHunk, kind: MarkKind) -> LiveHunks<'a> {
        let mut change = 0;
        let hunks = self
            .order
            .iter()
            .zip(&self.joined)
            .enumerate()
            .map(|(k, (&i, &joined_above))| {
                if k > 0 && !joined_above {
                    change += 1;
                }
                LiveHunk {
                    hunk: hunk(i),
                    rows: self.rows[i].clone(),
                    joined_above,
                    change,
                }
            })
            .collect();
        LiveHunks {
            hunks,
            staged: self.staged.clone(),
            kind,
        }
    }
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
            staged_marks: Arc::default(),
            staged_rows: Arc::default(),
            staged_tally,
            mark_kind: MarkKind::default(),
            version: Self::next_version(),
            resolved_rows: Arc::new(Mutex::new(None)),
        }
    }

    /// This map, with its staged state read as `kind`.
    pub(crate) fn with_mark_kind(mut self, kind: MarkKind) -> Self {
        self.mark_kind = kind;
        self
    }

    /// What the hunks' staged state means in this map.
    pub fn mark_kind(&self) -> MarkKind {
        self.mark_kind
    }

    /// Attach base-text syntax highlights for the diff view's left column.
    pub fn set_base_highlights(&mut self, highlights: Arc<BaseHighlights>) {
        self.base_highlights = Some(highlights);
    }

    /// Set the changes the base text holds over the text behind it.
    ///
    /// Each mark counts as a staged hunk in [`Self::staged_counts`], since it is
    /// a change already in the index. A second call replaces the first.
    pub(crate) fn set_staged_marks(&mut self, mut marks: Vec<StagedMark>) {
        marks.sort_by_key(|mark| (mark.base_lines.start, mark.base_lines.end));
        self.staged_tally.0 = self.staged_tally.0 - self.staged_marks.len() + marks.len();
        self.staged_marks = Arc::new(marks);
        self.staged_rows = Arc::new(
            self.staged_rows_at(self.hunks.iter().map(|hunk| hunk.buffer_line_range.clone())),
        );
    }

    /// [`Self::staged_marks`] as buffer rows, against hunks that sit at
    /// `hunk_rows`, one range per hunk in tree order.
    ///
    /// Empty when there are no marks, or no base text to place them against.
    fn staged_rows_at(
        &self,
        hunk_rows: impl Iterator<Item = Range<u32>>,
    ) -> Vec<(Range<u32>, DiffHunkStatus)> {
        let Some(base_text) = &self.base_text else {
            return Vec::new();
        };
        if self.staged_marks.is_empty() {
            return Vec::new();
        }

        let hunks: Vec<(Range<u32>, Range<u32>)> = self
            .hunks
            .iter()
            .zip(hunk_rows)
            .map(|(hunk, rows)| {
                (
                    rows,
                    hunk_base_lines(hunk, &self.base_line_starts, base_text),
                )
            })
            .collect();
        staged_mark_rows(&hunks, &self.staged_marks)
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
    ///
    /// `index_changed` must be sorted by start with non-decreasing ends, as the
    /// hunk list of that pass is.
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
            return held.answer(|i| hunks[i], self.mark_kind);
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
        let staged = self.staged_rows_at(rows.iter().cloned());
        let joined = joined_above(order.iter().map(|&i| (live[i].0, &rows[i])), buffer.rope());

        let held = cache.insert(LiveRowsCache {
            buffer_version,
            rows,
            order,
            joined,
            staged,
        });
        held.answer(|i| live[i].0, self.mark_kind)
    }

    pub fn version(&self) -> usize {
        self.version
    }

    /// Whether `other` would decorate the buffer exactly as this map does.
    ///
    /// Compares the hunks, the base text, the staged marks, and the mark kind,
    /// which is everything rendering reads. The base change spans, the staged
    /// map, the staged rows, and the staged tally are all derived from those,
    /// and the base highlights from the base text and the style table. The
    /// marks and the mark kind are set after construction rather than derived,
    /// so they are compared on their own.
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
        same_base
            && self.staged_marks == other.staged_marks
            && self.mark_kind == other.mark_kind
            && self.hunks.iter().eq(other.hunks.iter())
    }

    pub fn base_text(&self) -> Option<&Arc<String>> {
        self.base_text.as_ref()
    }

    /// The base text's line count, zero with no base text.
    ///
    /// A trailing newline counts the empty line after it, as a buffer's line
    /// count does, so the two compare line for line.
    pub(crate) fn base_lines(&self) -> u32 {
        self.base_line_starts.len() as u32
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
    /// hunk or staged mark touches it.
    ///
    /// A row inside a hunk's `buffer_line_range` reports that hunk's status. A
    /// row a [`DiffHunkStatus::Deleted`] hunk anchors -- its removed content
    /// rendered just above -- reports `Deleted`, the deletion seam. The state
    /// is the row's staged state for a contained row, or the whole hunk's for a
    /// deletion or move seam. A row no hunk marks answers the staged mark on
    /// it, if any. The map's [`MarkKind`] decides whether the state is a git
    /// state or an approval state.
    ///
    /// Reports whole hunk ranges in stored rows, each row with the tree pass's
    /// reading of it. The live counterpart [`LiveHunks::gutter_mark_for_line`]
    /// reads the rows the hunks cover now, which is what the render paths read.
    pub fn gutter_mark_for_line(&self, line: u32) -> Option<GutterMark> {
        self.hunk_mark_for_line(line)
            .or_else(|| staged_row_mark(&self.staged_rows, line))
            .map(|(status, tree, marked)| GutterMark {
                status,
                tree,
                state: MarkState::of(self.mark_kind, marked),
            })
    }

    /// The mark a hunk paints at buffer `line`, before the staged marks.
    fn hunk_mark_for_line(&self, line: u32) -> Option<(DiffHunkStatus, TreeMark, bool)> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        let hunk = cursor.item()?;
        if hunk.buffer_line_range.contains(&line) {
            return Some((hunk.status, hunk.tree_mark(line), hunk.line_staged(line)));
        }
        if hunk.status == DiffHunkStatus::Deleted && hunk.buffer_start_line == line {
            return Some((DiffHunkStatus::Deleted, hunk.tree_mark(line), hunk.staged()));
        }
        if hunk.status == DiffHunkStatus::Moved
            && hunk.buffer_line_range.is_empty()
            && hunk.buffer_start_line == line
        {
            return Some((DiffHunkStatus::Moved, hunk.tree_mark(line), hunk.staged()));
        }
        None
    }

    /// The staged state of the hunk that contains `line`, or `None` when no
    /// hunk or staged mark covers it.
    ///
    /// The map's [`MarkKind`] decides whether the state is a git state or an
    /// approval state. A row no hunk covers reads staged when a staged mark
    /// covers it. Deletions occupy no buffer rows, so no line resolves to one
    /// here.
    pub fn staged_for_line(&self, line: u32) -> Option<MarkState> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        cursor
            .item()
            .filter(|hunk| hunk.buffer_line_range.contains(&line))
            .map(|hunk| hunk.line_staged(line))
            .or_else(|| {
                let before = self
                    .staged_rows
                    .partition_point(|(rows, _)| rows.start <= line);
                self.staged_rows[..before]
                    .last()
                    .and_then(|(rows, _)| rows.contains(&line).then_some(true))
            })
            .map(|marked| MarkState::of(self.mark_kind, marked))
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
            && self.base_line_count(hunk) > 0
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
    /// live rows outrun its base rows and the left column is blank. An added
    /// hunk has no base rows at all, and a moved-to hunk paints none beside
    /// its rows, since its base lines belong to the seam it left.
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
                    DiffHunkStatus::Added | DiffHunkStatus::Moved => rows,
                    DiffHunkStatus::Modified if pair_modified => {
                        rows.saturating_sub(self.base_line_count(hunk))
                    },
                    DiffHunkStatus::Modified => rows,
                    DiffHunkStatus::Deleted => 0,
                }
            })
            .sum()
    }

    /// Base lines that the moved-away seams at buffer rows in `rows` removed.
    ///
    /// The left column paints no row for a seam, so a count of the base lines
    /// above a row adds these to the lines its rows show.
    pub(crate) fn seam_base_lines_in(&self, rows: Range<u32>) -> u32 {
        self.moved_away_seams()
            .filter(|(row, _)| rows.contains(row))
            .map(|(_, lines)| lines)
            .sum()
    }

    /// Every moved-away seam as the buffer row it sits before and the base
    /// lines it removed, in row order.
    ///
    /// A move leaves a seam where its lines left. The seam is a `Moved` hunk
    /// with no live rows, and its base range still holds the lines. The
    /// moved-to hunk carries the rows and the origin chip, so the seam's lines
    /// take no row on the left, and the left column's line count jumps past
    /// them.
    pub(crate) fn moved_away_seams(&self) -> impl Iterator<Item = (u32, u32)> {
        self.hunks
            .iter()
            .filter(|hunk| {
                hunk.status == DiffHunkStatus::Moved && hunk.buffer_line_range.is_empty()
            })
            .map(|hunk| (hunk.buffer_start_line, self.base_line_count(hunk)))
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
    /// Zero without base text, since no row pairs against text the map does not
    /// hold.
    ///
    /// The count is that of [`hunk_base_lines`], read from the base line-start
    /// table in two binary searches, so a paint that asks it for each row costs
    /// nothing that grows with the hunk's base text. A removed lone blank line
    /// counts one, and so does the blank line that a removed run ends in.
    pub(crate) fn base_line_count(&self, hunk: &DiffHunk) -> u32 {
        let Some(text) = self.base_text.as_ref() else {
            return 0;
        };
        hunk_base_lines(hunk, &self.base_line_starts, text).len() as u32
    }

    /// The output of [`Self::deleted_blocks`], reduced to what tells one
    /// refresh's set apart from the next.
    ///
    /// A diff recompute stamps a new version whether or not any hunk moved, so
    /// a caller that re-splices on the version alone re-splices constantly.
    /// Comparing the blocks themselves is not open to it, since
    /// [`BlockProperties`] carries a render closure. A [`DeletedBlockKey`]
    /// holds every input the blocks are built from, so equal signatures mean
    /// an identical set.
    ///
    /// The live range's end is among them because a Modified hunk pairs its
    /// base rows against its live ones, so a hunk that kept its start and its
    /// base bytes still blocks differently once its live row count moves. The
    /// wrap geometry is among them because it decides how many rows each base
    /// line breaks into, and the rows each index-paired live line takes
    /// because they decide where its base line overflows, or, in the unified
    /// layout, which base lines a fold hides.
    ///
    /// `live_rows` answers how many display rows a live buffer row takes.
    pub(crate) fn deleted_block_signature(
        &self,
        pair_modified: bool,
        wrap: Option<WrapGeometry>,
        live_rows: &dyn Fn(u32) -> u32,
    ) -> Vec<DeletedBlockKey> {
        if self.base_text.is_none() {
            return Vec::new();
        }
        self.deleted_block_hunks(pair_modified, wrap)
            .map(|hunk| DeletedBlockKey {
                status: hunk.status,
                buffer_start: hunk.buffer_start_line,
                buffer_end: hunk.buffer_line_range.end,
                base_bytes: hunk.base_byte_range.clone(),
                wrap,
                live_rows: match (wrap, pair_modified) {
                    (None, true) => Vec::new(),
                    _ => self.index_paired_lines(hunk).map(live_rows).collect(),
                },
            })
            .collect()
    }

    /// The hunks that render as deleted-line blocks, shared by
    /// [`Self::deleted_blocks`] and [`Self::deleted_block_signature`] so the
    /// signature cannot drift from the set it stands for.
    ///
    /// A hunk whose base rows all pair with live rows is not among them while
    /// nothing wraps. The paint puts those rows in the left column of the rows
    /// they pair with, which leaves nothing for a block to hold. Under `wrap`
    /// such a hunk stays, since a paired base line wider than its live line
    /// overflows into a block.
    fn deleted_block_hunks(
        &self,
        pair_modified: bool,
        wrap: Option<WrapGeometry>,
    ) -> impl Iterator<Item = &DiffHunk> {
        self.hunks.iter().filter(move |hunk| {
            let paired = self.paired_rows(hunk, pair_modified);
            matches!(
                hunk.status,
                DiffHunkStatus::Deleted | DiffHunkStatus::Modified
            ) && (self.base_line_count(hunk) > paired || (wrap.is_some() && paired > 0))
        })
    }

    /// The live rows of a Modified `hunk` that index-pair with a base line, as
    /// far as the shorter side reaches. Empty for any other hunk.
    ///
    /// The two-column layout paints each of those base lines beside its row.
    /// The unified layout blocks them, and a fold that hides a row hides its
    /// line too.
    fn index_paired_lines(&self, hunk: &DiffHunk) -> Range<u32> {
        let count = match hunk.status {
            DiffHunkStatus::Modified => self
                .base_line_count(hunk)
                .min(hunk.buffer_line_range.len() as u32),
            _ => 0,
        };
        hunk.buffer_start_line..hunk.buffer_start_line + count
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
    ///
    /// In the unified layout a Modified hunk's base line index-pairs with a live
    /// row. A fold over live rows hides the base lines index-paired with them,
    /// each run of shown rows blocks its lines above its first row, and the
    /// excess follows the last live row. A row a fold hides answers `u32::MAX`
    /// from `live_rows`.
    ///
    /// Each base line is a block of its own, and the blocks come grouped by
    /// hunk in the order [`Self::deleted_block_signature`] lists the hunks.
    /// Under `wrap`, a line wider than the width takes a row per segment,
    /// broken where the same text breaks as a live line, with each
    /// continuation indented as a live continuation is.
    ///
    /// A paired base line breaks too, a segment beside each row of its live
    /// line. The segments its live line has no row for block below that live
    /// row, ahead of any excess base lines there, and carry on the line rather
    /// than open one (see [`BlockProperties::first_segment`]). `live_rows`
    /// answers how many display rows a live buffer row takes.
    pub(crate) fn deleted_blocks(
        &self,
        pair_modified: bool,
        wrap: Option<WrapGeometry>,
        live_rows: &dyn Fn(u32) -> u32,
    ) -> Vec<Vec<BlockProperties>> {
        let base_text = match &self.base_text {
            Some(t) => t,
            None => return Vec::new(),
        };

        self.deleted_block_hunks(pair_modified, wrap)
            .map(|hunk| {
                let paired = self.paired_rows(hunk, pair_modified);
                if hunk.status == DiffHunkStatus::Modified && paired == 0 {
                    let lines = self.removed_base_lines(hunk, base_text);
                    return unified_modified_blocks(hunk, lines, wrap, live_rows);
                }
                let placement_line = match hunk.status {
                    DiffHunkStatus::Modified if paired > 0 => hunk.buffer_line_range.end - 1,
                    _ => hunk.buffer_start_line.saturating_sub(1),
                };
                let mut lines = self.removed_base_lines(hunk, base_text);
                let mut blocks = Vec::new();
                for (row, line) in
                    (hunk.buffer_start_line..).zip(lines.by_ref().take(paired as usize))
                {
                    if wrap.is_none() {
                        continue;
                    }
                    let segments = line_segments(line, wrap);
                    let shown = live_rows(row) as usize;
                    if segments.len() > shown {
                        blocks.push(base_line_block(
                            BlockPlacement::Below(row),
                            segments[shown..].to_vec(),
                            shown as u32,
                            hunk.status,
                        ));
                    }
                }
                blocks.extend(lines.map(|line| {
                    base_line_block(
                        BlockPlacement::Below(placement_line),
                        line_segments(line, wrap),
                        0,
                        hunk.status,
                    )
                }));
                blocks
            })
            .collect()
    }

    /// The text of each base line `hunk` removed, without its line ending.
    ///
    /// The lines are the ones [`hunk_base_lines`] names, so a removed blank
    /// line yields an empty string rather than nothing.
    fn removed_base_lines<'a>(&self, hunk: &DiffHunk, base_text: &'a str) -> Lines<'a> {
        let lines = hunk_base_lines(hunk, &self.base_line_starts, base_text);
        let start_of = |line: u32| {
            self.base_line_starts
                .get(line as usize)
                .copied()
                .unwrap_or(base_text.len())
        };
        base_text[start_of(lines.start)..start_of(lines.end)].lines()
    }

    /// How many hunks the map holds.
    ///
    /// A staged mark does not count. The status bar's position counts changes
    /// through [`LiveHunks::change_count`] instead, which folds hunks that only
    /// whitespace-only rows separate into one.
    pub fn hunk_count(&self) -> usize {
        self.hunks.iter().count()
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

    /// The staged state of the hunk that removed content at base `line`, and
    /// the tree pass's reading of that hunk's first buffer row, or `None` when
    /// no hunk did.
    ///
    /// Serves the diff view's removed left-column rows, whose base lines have no
    /// buffer counterpart for [`Self::staged_for_line`] to resolve.
    ///
    /// The map's [`MarkKind`] decides whether the state is a git state or an
    /// approval state.
    pub(crate) fn base_line_mark(&self, line: u32) -> Option<(MarkState, TreeMark)> {
        self.base_staged
            .get(&line)
            .map(|mark| (MarkState::of(self.mark_kind, mark.staged), mark.tree))
    }

    /// Whether the hunk that replaced base `line` names the line's changed
    /// chars, so the chars outside its spans are unchanged text.
    ///
    /// `false` when no hunk covers `line`, and when the hunk removed the line
    /// whole.
    pub(crate) fn base_line_refined(&self, line: u32) -> bool {
        self.base_staged.get(&line).is_some_and(|mark| mark.refined)
    }

    /// The mark the staged mark at base `line` paints, or `None` when no mark
    /// covers it.
    ///
    /// What the diff view's left column marks at `line` beyond the removed rows
    /// [`Self::base_line_mark`] covers. A deletion's empty mark answers at the
    /// line it sits at, the line that followed the removed text. A mark reads
    /// staged under the map's [`MarkKind`], as a staged gutter row does, and
    /// reads [`TreeMark::Unrefined`], because only a line diff produces it.
    pub(crate) fn staged_mark_at_base_line(&self, line: u32) -> Option<GutterMark> {
        let end = self
            .staged_marks
            .partition_point(|mark| mark.base_lines.start <= line);
        self.staged_marks[..end]
            .iter()
            .rev()
            .take_while(|mark| mark.base_lines.end >= line)
            .find(|mark| {
                mark.base_lines.contains(&line)
                    || (mark.base_lines.is_empty() && mark.base_lines.start == line)
            })
            .map(|mark| GutterMark {
                status: mark.status,
                tree: TreeMark::Unrefined,
                state: MarkState::of(self.mark_kind, true),
            })
    }

    pub fn total_deleted_lines(&self) -> u32 {
        self.hunks
            .iter()
            .filter(|h| matches!(h.status, DiffHunkStatus::Deleted | DiffHunkStatus::Modified))
            .map(|h| self.base_line_count(h))
            .sum()
    }

    #[cfg(test)]
    pub fn set_base_text(&mut self, text: Arc<String>) {
        self.base_line_starts = Arc::new(line_starts(&text));
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

/// The rows a deleted base line takes in its block, broken under `wrap` with
/// each continuation indented as a live continuation is.
fn line_segments(line: &str, wrap: Option<WrapGeometry>) -> Vec<String> {
    let Some(wrap) = wrap else {
        return vec![line.to_string()];
    };
    let (windows, indent) = wrap.windows(line);
    windows
        .into_iter()
        .enumerate()
        .map(|(row, window)| match row {
            0 => line[window].to_string(),
            _ => format!("{}{}", " ".repeat(indent as usize), &line[window]),
        })
        .collect()
}

/// The blocks of a Modified `hunk`'s base `lines` in the unified layout, which
/// index-pairs each base line with a live row.
///
/// A base line hides with its live row when a fold hides that row, the way a
/// paired base line in the two-column layout hides with the row it paints
/// beside. Each run of shown rows blocks its lines above its first row, so
/// every kept run reads as removed lines over added lines. The lines past the
/// live side join the run that holds the last live row, or hang below that row
/// when a fold hides it, and the block map moves them past the placeholder.
///
/// `live_rows` answers `u32::MAX` for a row a fold hides, per
/// [`DiffMap::deleted_blocks`].
fn unified_modified_blocks(
    hunk: &DiffHunk,
    mut lines: Lines<'_>,
    wrap: Option<WrapGeometry>,
    live_rows: &dyn Fn(u32) -> u32,
) -> Vec<BlockProperties> {
    let rows = hunk.buffer_line_range.clone();
    let mut blocks = Vec::new();
    let mut run = None;
    for (row, line) in rows.clone().zip(lines.by_ref()) {
        if live_rows(row) == u32::MAX {
            run = None;
            continue;
        }
        let placement = *run.get_or_insert(BlockPlacement::Below(row.saturating_sub(1)));
        blocks.push(base_line_block(
            placement,
            line_segments(line, wrap),
            0,
            hunk.status,
        ));
    }

    let excess = run.unwrap_or(BlockPlacement::Below(rows.end.saturating_sub(1)));
    blocks.extend(
        lines.map(|line| base_line_block(excess, line_segments(line, wrap), 0, hunk.status)),
    );
    blocks
}

/// A block of `rows` from one base line, the first of them its segment
/// `first_segment`.
fn base_line_block(
    placement: BlockPlacement,
    rows: Vec<String>,
    first_segment: u32,
    status: DiffHunkStatus,
) -> BlockProperties {
    let mut props = BlockProperties::from_text(placement, rows, BlockStyle::Fixed);
    props.diff_status = Some(status);
    props.first_segment = first_segment;
    props
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
///
/// `index_changed` must be sorted by start with non-decreasing ends, as the
/// hunk list of a diff is. Each hunk then searches for the ranges that reach
/// it, rather than testing every range.
pub(crate) fn mark_staged(hunks: &mut [DiffHunk], index_changed: &[Range<u32>]) {
    debug_assert!(
        index_changed
            .windows(2)
            .all(|w| w[0].start <= w[1].start && w[0].end <= w[1].end),
        "index ranges are sorted by start with non-decreasing ends",
    );
    for hunk in hunks.iter_mut() {
        let range = hunk.buffer_line_range.clone();
        // A range that overlaps the hunk ends at or after its start and starts
        // at or before its end, a zero-width range at either edge included. So
        // with both bounds sorted, every overlap sits in this one window. Each
        // hunk searches on its own, because a range that spans two hunks
        // reaches both.
        let from = index_changed.partition_point(|c| c.end < range.start);
        let mut reaching = index_changed[from..]
            .iter()
            .take_while(|c| c.start <= range.end);
        if range.start == range.end {
            let unstaged = reaching.any(|c| ranges_overlap(c, &range));
            hunk.unstaged_lines = match unstaged {
                true => vec![range],
                false => Vec::new(),
            };
        } else {
            hunk.unstaged_lines = reaching
                .filter(|c| ranges_overlap(c, &range))
                .map(|c| c.start.max(range.start)..c.end.min(range.end))
                .collect();
        }
    }
}

/// Record which of each hunk's rows a range review did not approve.
///
/// `approved` is sorted and disjoint, as
/// [`ApprovedRows::ranges`](crate::review_approval::ApprovedRows::ranges)
/// keeps it. A zero-width hunk at row `a` reads approved when `a..a + 1` is
/// approved. Every other hunk keeps as unstaged the rows of its range that no
/// approved range covers, so a hunk approved whole reads staged.
pub(crate) fn mark_approved(hunks: &mut [DiffHunk], approved: &[Range<u32>]) {
    for hunk in hunks.iter_mut() {
        let range = hunk.buffer_line_range.clone();
        if range.start == range.end {
            let row = range.start..range.start + 1;
            let covered = approved
                .iter()
                .any(|rows| rows.start <= row.start && row.end <= rows.end);
            hunk.unstaged_lines = match covered {
                true => Vec::new(),
                false => vec![range],
            };
            continue;
        }

        let mut unstaged = Vec::new();
        let mut next = range.start;
        for rows in approved
            .iter()
            .filter(|rows| rows.start < range.end && range.start < rows.end)
        {
            if next < rows.start {
                unstaged.push(next..rows.start);
            }
            next = next.max(rows.end);
        }
        if next < range.end {
            unstaged.push(next..range.end);
        }
        hunk.unstaged_lines = unstaged;
    }
}

/// The staged marks `base` holds over `from`, the text behind it.
///
/// From the line pass alone. A mark paints whole lines, so the token detail the
/// tree pass adds has nothing to refine.
pub(crate) fn staged_marks(from: &str, base: &str) -> Vec<StagedMark> {
    changes_to_hunks(
        &stoat_language::structural_diff::diff(from, base).changes,
        from,
        base,
    )
    .into_iter()
    .map(|hunk| StagedMark {
        status: hunk.status,
        base_lines: hunk.buffer_line_range,
    })
    .collect()
}

/// Map `marks`, in base-line coordinates, onto the buffer rows they paint on.
///
/// `hunks` pairs each hunk's buffer rows with the base lines it replaced, in
/// base order. A base line outside every hunk sits at the buffer row the hunks
/// before it shift it to. A mark's lines inside a hunk's base range are
/// dropped, because those rows differ from the base and the hunk's own mark is
/// the truth there. An empty mark maps to the row its line shifts to, unless a
/// hunk's base range holds that line.
///
/// The answer is sorted by row, with one range per unbroken run of a mark's
/// rows.
pub(crate) fn staged_mark_rows(
    hunks: &[(Range<u32>, Range<u32>)],
    marks: &[StagedMark],
) -> Vec<(Range<u32>, DiffHunkStatus)> {
    let mut out = Vec::with_capacity(marks.len());
    let mut next = 0;
    let mut delta: i64 = 0;

    for mark in marks {
        if mark.base_lines.is_empty() {
            let line = mark.base_lines.start;
            if !fold_to(hunks, line, &mut next, &mut delta) {
                let row = shift(line, delta);
                out.push((row..row, mark.status));
            }
            continue;
        }

        let mut run: Option<Range<u32>> = None;
        for line in mark.base_lines.clone() {
            if fold_to(hunks, line, &mut next, &mut delta) {
                out.extend(run.take().map(|rows| (rows, mark.status)));
                continue;
            }
            let row = shift(line, delta);
            run = match run {
                Some(rows) if rows.end == row => Some(rows.start..row + 1),
                open => {
                    out.extend(open.map(|rows| (rows, mark.status)));
                    Some(row..row + 1)
                },
            };
        }
        out.extend(run.map(|rows| (rows, mark.status)));
    }

    out.sort_by_key(|(rows, _)| (rows.start, rows.end));
    out
}

/// Fold every hunk from `next` that ends at or before base `line` into
/// `delta`, the row shift those hunks put on the lines after them, and answer
/// whether the first hunk left holds `line`.
fn fold_to(
    hunks: &[(Range<u32>, Range<u32>)],
    line: u32,
    next: &mut usize,
    delta: &mut i64,
) -> bool {
    while let Some((rows, base)) = hunks.get(*next)
        && base.end <= line
    {
        *delta += (i64::from(rows.end) - i64::from(rows.start))
            - (i64::from(base.end) - i64::from(base.start));
        *next += 1;
    }
    hunks
        .get(*next)
        .is_some_and(|(_, base)| base.contains(&line))
}

/// Base `line` as a buffer row, after the shift `delta` of the hunks above it.
fn shift(line: u32, delta: i64) -> u32 {
    (i64::from(line) + delta).max(0) as u32
}

/// The gutter mark that `rows`, staged marks as buffer rows sorted by row,
/// holds at buffer `line`.
///
/// A range that holds `line` answers its status, and an empty range at `line`
/// answers [`DiffHunkStatus::Deleted`], where a staged removal took its text.
/// Both read staged, and both read [`TreeMark::Unrefined`], because only a
/// line diff produces a staged mark.
fn staged_row_mark(
    rows: &[(Range<u32>, DiffHunkStatus)],
    line: u32,
) -> Option<(DiffHunkStatus, TreeMark, bool)> {
    let (range, status) = rows[..rows.partition_point(|(range, _)| range.start <= line)].last()?;
    if range.contains(&line) {
        return Some((*status, TreeMark::Unrefined, true));
    }
    (range.is_empty() && range.start == line).then_some((
        DiffHunkStatus::Deleted,
        TreeMark::Unrefined,
        true,
    ))
}

/// The live rows from `hunk`'s first marked row to its last, where `rows` are
/// the rows the hunk covers now. A hunk the tree pass never narrowed marks all
/// of `rows`.
fn marked_span(hunk: &DiffHunk, rows: &Range<u32>) -> Range<u32> {
    let (Some(first), Some(last)) = (hunk.marked_rows.first(), hunk.marked_rows.last()) else {
        return rows.clone();
    };
    // The runs are the hunk's own coordinates, so they shift with it the way
    // its live rows did. An edit inside the hunk leaves them stale, and the
    // clamp keeps a stale run inside the live rows.
    let delta = i64::from(rows.start) - i64::from(hunk.buffer_line_range.start);
    let live = |row: u32| shift(row, delta).clamp(rows.start, rows.end);
    live(first.start)..live(last.end)
}

/// Whether each of `hunks`, given in answer order with the rows each covers
/// now, joins the hunk above it into one change.
///
/// Two hunks join when no row between the first one's last marked row and the
/// second one's first holds text. A blank line between two changed blocks
/// splits the diff into two hunks, but the reader sees one change there.
/// Touching or overlapping hunks join too. The first hunk joins nothing.
fn joined_above<'a>(
    hunks: impl Iterator<Item = (&'a DiffHunk, &'a Range<u32>)>,
    rope: &Rope,
) -> Vec<bool> {
    let mut above: Option<u32> = None;
    hunks
        .map(|(hunk, rows)| {
            let marked = marked_span(hunk, rows);
            let joined =
                above.is_some_and(|end| (end..marked.start).all(|row| !row_holds_text(rope, row)));
            above = Some(marked.end);
            joined
        })
        .collect()
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
                    prose: ProseChange::None,
                })
                .collect();
            let base_spans = lhs_indices
                .iter()
                .map(|i| ChangeSpan {
                    byte_range: changes[*i].byte_range.clone(),
                    kind: ChangeKind::Moved,
                    move_metadata: metadata.clone(),
                    prose: ProseChange::None,
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
                    tree: false,
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
                    prose: ProseChange::None,
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
                    tree: false,
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
        let line_range = change_line_range(&rhs_starts, rhs_text.len(), &rhs_change.byte_range);
        hunks.push(DiffHunk {
            status: DiffHunkStatus::Modified,
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: lhs_change.byte_range.clone(),
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: replaced_change_spans(rhs_change, rhs_text),
                base_spans: replaced_change_spans(lhs_change, lhs_text),
                tree: false,
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
                let line_range = change_line_range(&rhs_starts, rhs_text.len(), &cur.byte_range);
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
/// A hunk that reaches a section in `unsearched`, the sections the tree search
/// gave up on, keeps the detail, status, and marked rows that
/// [`changes_to_hunks`] gave it. The tree changes there are whole `Novel` runs
/// and the move pass's guesses, which wash far more than the line pass's char
/// refinement.
///
/// A `Modified` hunk the tree pass finds nothing in takes an empty
/// [`TokenDetail`] rather than `None`. The renderer washes nothing and softens
/// nothing for an empty span list, which is the intended reading of a
/// whitespace-only row: full strength, wash-free, and [`TreeMark::Unmarked`],
/// so the plain gutter leaves the row out. `None` means "no detail resolved"
/// instead, and leaves the row to the caller's default.
///
/// `Added` and `Deleted` hunks keep `None` unless the tree pass found the lines
/// relocated, which is the one thing the line pass cannot see. A hunk the tree
/// pass covers entirely becomes [`DiffHunkStatus::Moved`] and takes the spans
/// naming where the content came from; a hunk it covers in part keeps its
/// status and takes them anyway, since the renderer tints a `Moved` span with
/// the move color wherever it sits. A hunk it does not reach keeps `None`,
/// which is what suppresses a whole-line wash on it.
///
/// Spans stay file-absolute, exactly as [`replaced_change_spans`] leaves them.
pub(crate) fn merge_structural_detail(
    hunks: &mut [DiffHunk],
    tree_changes: &[stoat_language::structural_diff::DiffChange],
    unsearched: &[(Range<usize>, Range<usize>)],
    base_text: &str,
    buffer_text: &str,
) {
    use stoat_language::structural_diff::{ChangeKind as LangChangeKind, Side};

    let buffer_starts = line_starts(buffer_text);
    let base_starts = line_starts(base_text);
    // Read once per change up front. A change across several lines reaches
    // each hunk on them, and the read scans the change's text.
    let prose_by_change: Vec<ProseChange> = tree_changes
        .iter()
        .map(|change| match change.side {
            Side::Rhs => prose_change(change, buffer_text),
            Side::Lhs => prose_change(change, base_text),
        })
        .collect();
    // On one side the tree changes are disjoint atom runs sorted by start, so
    // their starts and their ends both ascend. Each hunk then searches for the
    // run of changes that reaches it, rather than testing every change.
    let (lhs, rhs): (Vec<usize>, Vec<usize>) =
        (0..tree_changes.len()).partition(|&i| tree_changes[i].side == Side::Lhs);
    debug_assert!(
        [&lhs, &rhs]
            .into_iter()
            .all(|side| side.windows(2).all(|pair| {
                tree_changes[pair[0]].byte_range.end <= tree_changes[pair[1]].byte_range.end
            })),
        "each side's tree changes end in ascending order",
    );
    for hunk in hunks.iter_mut() {
        let refining = hunk.status == DiffHunkStatus::Modified;
        if !refining && !matches!(hunk.status, DiffHunkStatus::Added | DiffHunkStatus::Deleted) {
            continue;
        }
        let buffer_bytes =
            line_range_to_byte_range(&buffer_starts, buffer_text.len(), &hunk.buffer_line_range);
        let overlaps = |a: &Range<usize>, b: &Range<usize>| a.start < b.end && b.start < a.end;
        if unsearched
            .iter()
            .any(|(lhs, rhs)| overlaps(lhs, &hunk.base_byte_range) || overlaps(rhs, &buffer_bytes))
        {
            continue;
        }
        let mut buffer_spans = Vec::new();
        let mut base_spans = Vec::new();
        let mut moved_ranges = Vec::new();
        let reaching = [
            changes_reaching(&rhs, tree_changes, &buffer_bytes),
            changes_reaching(&lhs, tree_changes, &hunk.base_byte_range),
        ];
        for &i in reaching.into_iter().flatten() {
            let (change, prose) = (&tree_changes[i], prose_by_change[i]);
            let moved = change.kind == LangChangeKind::Moved;
            // A relocation that stays inside a Modified hunk is noise. The tree
            // differ reports every token of a block reindented under a new
            // parent as moved, and a wash of those marks a run of untouched
            // statements more heavily than the line differ does. A move whose
            // counterpart lies outside the hunk is a token that left this line
            // or arrived on it, which the reader sees here.
            if moved
                && refining
                && counterpart_in_hunk(change, &hunk.base_byte_range, &buffer_bytes)
            {
                continue;
            }
            let (bounds, out) = match change.side {
                Side::Rhs => (&buffer_bytes, &mut buffer_spans),
                Side::Lhs => (&hunk.base_byte_range, &mut base_spans),
            };
            let overlapping = effective_ranges(change)
                .iter()
                .filter(|range| range.start < bounds.end && bounds.start < range.end);
            // Kept at full width: the cover below asks only which of the hunk's
            // own lines a range reaches, so how far past the hunk it runs says
            // nothing. Only the side this hunk is about contributes at all --
            // an Added hunk carries no base range and a Deleted one a
            // zero-width buffer anchor, so the other side overlaps nothing.
            if moved {
                moved_ranges.extend(overlapping.clone().cloned());
            }
            out.extend(overlapping.map(|range| ChangeSpan {
                byte_range: range.clone(),
                kind: match moved {
                    true => ChangeKind::Moved,
                    false => span_kind(change),
                },
                move_metadata: change.move_metadata.clone(),
                prose,
            }));
        }

        // A hunk the relocation accounts for line by line is the other half of
        // a move, and the line pass had no way to know it. One it accounts for
        // in part is still an addition or a deletion, and keeps its status
        // while the spans inside it tint as moved.
        if !refining && !moved_ranges.is_empty() {
            let (text, starts, lines) = match hunk.status {
                DiffHunkStatus::Deleted => (
                    base_text,
                    &base_starts,
                    byte_range_to_line_range(&base_starts, base_text.len(), &hunk.base_byte_range),
                ),
                _ => (buffer_text, &buffer_starts, hunk.buffer_line_range.clone()),
            };
            if lines_covered(&moved_ranges, starts, text, lines) {
                hunk.status = DiffHunkStatus::Moved;
            }
        }

        hunk.marked_rows = marked_row_runs(
            &buffer_spans,
            |off| line_of(&buffer_starts, off.min(buffer_text.len())),
            |row| line_is_blank(&buffer_starts, buffer_text, row),
        );
        hunk.token_detail = Some(Arc::new(TokenDetail {
            buffer_spans,
            base_spans,
            tree: true,
        }));
    }
}

/// The indices in `side` whose change's byte range reaches `bounds`.
///
/// `side` lists one side's changes with their starts and their ends in
/// ascending order. So the changes that reach `bounds` form one run, from the
/// first that ends past `bounds.start` to the last that starts before
/// `bounds.end`. A change that strictly holds an empty `bounds` reaches it.
fn changes_reaching<'a>(
    side: &'a [usize],
    changes: &[stoat_language::structural_diff::DiffChange],
    bounds: &Range<usize>,
) -> &'a [usize] {
    let first = side.partition_point(|&i| changes[i].byte_range.end <= bounds.start);
    let len = side[first..].partition_point(|&i| changes[i].byte_range.start < bounds.end);
    &side[first..first + len]
}

/// Whether the moved `change` has a counterpart inside the hunk whose base side
/// spans `base_bytes` and whose buffer side spans `buffer_bytes`.
///
/// The move pass lists each side's counterparts on that side's own metadata. A
/// buffer-side target names its base-side sources, and a base-side source names
/// its buffer-side targets. A counterpart in another file is never in the hunk,
/// and a change with no metadata has none to find.
fn counterpart_in_hunk(
    change: &stoat_language::structural_diff::DiffChange,
    base_bytes: &Range<usize>,
    buffer_bytes: &Range<usize>,
) -> bool {
    use stoat_language::structural_diff::Side;

    change.move_metadata.as_ref().is_some_and(|meta| {
        meta.sources.iter().any(|source| {
            let bounds = match source.side {
                Side::Lhs => base_bytes,
                Side::Rhs => buffer_bytes,
            };
            source.buffer.is_none()
                && source.byte_range.start < bounds.end
                && bounds.start < source.byte_range.end
        })
    })
}

/// Whether every line of `lines` carrying content is touched by one of
/// `ranges`.
///
/// Blank lines are skipped rather than counted against the cover. The tree
/// differ marks tokens, and a line holding none can never be reached, so
/// requiring it would let the blank separator between two relocated functions
/// decide that neither of them moved.
fn lines_covered(ranges: &[Range<usize>], starts: &[usize], text: &str, lines: Range<u32>) -> bool {
    lines
        .filter(|line| !line_is_blank(starts, text, *line))
        .all(|line| {
            let bounds = line_range_to_byte_range(starts, text.len(), &(line..line + 1));
            ranges
                .iter()
                .any(|range| range.start < bounds.end && bounds.start < range.end)
        })
}

/// Whether buffer `row` holds a non-whitespace char.
pub(crate) fn row_holds_text(rope: &Rope, row: u32) -> bool {
    let start = rope.point_to_offset(Point::new(row, 0));
    rope.chars_at(start)
        .take_while(|&ch| ch != '\n')
        .any(|ch| !ch.is_whitespace())
}

/// Whether line `line` holds nothing but whitespace.
fn line_is_blank(starts: &[usize], text: &str, line: u32) -> bool {
    let bounds = line_range_to_byte_range(starts, text.len(), &(line..line + 1));
    text[bounds].trim().is_empty()
}

/// The byte ranges a change actually marks.
///
/// `refined_spans` when the differ refined the change, else the whole
/// `byte_range`, the same preference [`replaced_change_spans`] makes. A present
/// empty `refined_spans` marks nothing, so the change's text paints unchanged.
fn effective_ranges(change: &stoat_language::structural_diff::DiffChange) -> &[Range<usize>] {
    match &change.refined_spans {
        None => std::slice::from_ref(&change.byte_range),
        Some(spans) => spans.as_slice(),
    }
}

/// How a tree change washes.
///
/// Only the two content kinds reach this. A moved change takes
/// [`ChangeKind::Moved`] at the call site, where whether it is a move at all
/// has already been decided.
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
/// actually differ -- so a one-word edit records only that word. An absent
/// `refined_spans` means the refine never narrowed the change, so the whole
/// `byte_range` becomes the single span and a full rewrite still marks
/// completely. A present empty one yields no span, so the side's text paints
/// unchanged.
///
/// `text` is the full text of the change's side, per [`prose_change`].
fn replaced_change_spans(
    change: &stoat_language::structural_diff::DiffChange,
    text: &str,
) -> Vec<ChangeSpan> {
    let prose = prose_change(change, text);
    let ranges = match &change.refined_spans {
        None => std::slice::from_ref(&change.byte_range),
        Some(spans) => spans.as_slice(),
    };
    ranges
        .iter()
        .map(|range| ChangeSpan {
            byte_range: range.clone(),
            kind: ChangeKind::Replaced,
            move_metadata: None,
            prose,
        })
        .collect()
}

/// How much of its unstructured text `change` rewrote, read against `text`,
/// the full text of the change's side.
///
/// A change rewrote a part of its text when a letter or a digit of its range
/// sits outside the chars that differ. A comment marker, a quote, or a
/// separator carries no content of its own, so one that survived does not
/// count. Only a prose replacement is ever a part. An added or deleted run has
/// no counterpart to keep chars against.
fn prose_change(change: &stoat_language::structural_diff::DiffChange, text: &str) -> ProseChange {
    use stoat_language::structural_diff::ChangeKind as LangChangeKind;
    if !change.prose {
        return ProseChange::None;
    }
    if change.kind != LangChangeKind::Replaced {
        return ProseChange::Whole;
    }

    let keeps_a_word = |kept: Range<usize>| {
        text.get(kept)
            .is_some_and(|kept| kept.chars().any(char::is_alphanumeric))
    };
    let mut kept_from = change.byte_range.start;
    for changed in effective_ranges(change) {
        if keeps_a_word(kept_from..changed.start) {
            return ProseChange::Part;
        }
        kept_from = changed.end;
    }
    match keeps_a_word(kept_from..change.byte_range.end) {
        true => ProseChange::Part,
        false => ProseChange::Whole,
    }
}

/// Map each base line a hunk removed to that hunk's [`BaseLineMark`].
///
/// The lines come from [`hunk_base_lines`], so a removed blank line maps too,
/// and an `Added` hunk maps none.
///
/// A line is refined when its hunk is `Modified` and carries token detail, an
/// empty one included. [`merge_structural_detail`] gives every `Modified` hunk
/// a detail, and an empty one means a whitespace-only change, so no char of the
/// line changed. A `Deleted` or `Moved` hunk, and a `Modified` hunk with no
/// detail, removed the line whole.
fn compute_base_staged(hunks: &SumTree<DiffHunk>, base_text: Option<&Arc<String>>) -> BaseStaged {
    let Some(base_text) = base_text else {
        return BTreeMap::new();
    };
    let starts = line_starts(base_text);
    let mut out = BTreeMap::new();
    for hunk in hunks.iter() {
        let mark = BaseLineMark {
            staged: hunk.staged(),
            refined: hunk.status == DiffHunkStatus::Modified && hunk.token_detail.is_some(),
            tree: hunk.tree_mark(hunk.buffer_line_range.start),
        };
        for line in hunk_base_lines(hunk, &starts, base_text) {
            out.insert(line, mark);
        }
    }
    out
}

/// The base lines `hunk` removed or replaced, against the [`line_starts`] of
/// `base_text`.
///
/// The line differ ends a base range before its last line's terminator, so the
/// last line is the one the end byte sits on. An empty range on a `Deleted` or
/// `Modified` hunk is one blank line. On any other hunk an empty range covers no
/// line.
///
/// A whole-file removal's range runs past the final newline to the end of the
/// text. The empty tail after that newline holds no line, so the range stops
/// at the text's last line.
pub(crate) fn hunk_base_lines(hunk: &DiffHunk, starts: &[usize], base_text: &str) -> Range<u32> {
    let range = &hunk.base_byte_range;
    let start = line_of(starts, range.start);
    if range.is_empty() {
        let removed = matches!(
            hunk.status,
            DiffHunkStatus::Deleted | DiffHunkStatus::Modified
        );
        return start..start + u32::from(removed);
    }
    let last = line_of(starts, range.end.min(base_text.len().saturating_sub(1)));
    start..last + 1
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
                (span.kind.clone(), span.prose),
                &starts,
                base_text.len(),
            );
        }
    }
    out
}

/// Split an absolute base-text byte `range` into per-line-local ranges, pushing
/// each onto `out` under its base line with `mark`, the span's kind and prose
/// state.
///
/// `line_starts` gives each base line's byte offset, and `text_len` closes the
/// last line. A range spanning several lines contributes one clamped sub-range
/// per line it covers, with the trailing newline excluded.
fn distribute_change_span(
    out: &mut BaseChangeSpans,
    range: &Range<usize>,
    mark: (ChangeKind, ProseChange),
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
            out.entry(line as u32).or_default().push((
                (start - line_start)..(end - line_start),
                mark.0.clone(),
                mark.1,
            ));
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

/// The rows a line-differ change covers, from its byte range.
///
/// The line differ ends a run before its last line's terminator, so a run of
/// one blank line is the only change with an empty range, and it covers one
/// row.
fn change_line_range(
    line_starts: &[usize],
    text_len: usize,
    byte_range: &Range<usize>,
) -> Range<u32> {
    let rows = byte_range_to_line_range(line_starts, text_len, byte_range);
    if rows.is_empty() {
        return rows.start..rows.start + 1;
    }
    rows
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
    use super::{
        ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, GutterMark, MarkKind, MarkState,
        ProseChange, StagedMark, TokenDetail, TreeMark,
    };
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{BlockPlacement, BlockProperties, WrapGeometry},
        host::DiffStatus,
        multi_buffer::MultiBuffer,
    };
    use std::sync::Arc;
    use stoat_language::structural_diff::{ChangeKind as LangChangeKind, DiffChange, Side};

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
        let multi = MultiBuffer::singleton(shared.clone());

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
        let multi = MultiBuffer::singleton(shared.clone());

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
        let multi = MultiBuffer::singleton(shared.clone());

        let mut dm = DiffMap::from_hunks([added_hunk(1..2)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        let live = dm.live_hunks(&multi.snapshot());
        assert_eq!(
            live.gutter_mark_for_line(1),
            Some(unrefined(DiffHunkStatus::Added, MarkState::Unstaged)),
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
        let multi = MultiBuffer::singleton(shared.clone());

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
            Some(unrefined(DiffHunkStatus::Deleted, MarkState::Unstaged)),
            "and the deletion seam keeps its mark",
        );
        assert_eq!(
            live.gutter_mark_for_line(5),
            Some(unrefined(DiffHunkStatus::Moved, MarkState::Unstaged)),
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
        let multi = MultiBuffer::singleton(shared.clone());

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
            Some(unrefined(DiffHunkStatus::Deleted, MarkState::Unstaged)),
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

    fn mark(status: DiffHunkStatus, base_lines: std::ops::Range<u32>) -> StagedMark {
        StagedMark { status, base_lines }
    }

    /// A map over `base` holding `hunks`, with `marks` as its staged marks.
    fn staged_map(base: &str, hunks: Vec<DiffHunk>, marks: Vec<StagedMark>) -> DiffMap {
        let mut dm = DiffMap::from_hunks(hunks, Some(Arc::new(base.to_string())));
        dm.set_staged_marks(marks);
        dm
    }

    fn buffer_holding(text: &str) -> MultiBuffer {
        MultiBuffer::singleton(Arc::new(
            TextBuffer::with_text(BufferId::new(0), text).into(),
        ))
    }

    /// The gutter marks `dm` paints on `rows` of a buffer holding `text`,
    /// checked to agree between the live and the stored lookups.
    fn staged_gutter(
        dm: &DiffMap,
        text: &str,
        rows: std::ops::Range<u32>,
    ) -> Vec<Option<GutterMark>> {
        let multi = buffer_holding(text);
        let snapshot = multi.snapshot();
        let live = dm.live_hunks(&snapshot);
        rows.map(|row| {
            let mark = live.gutter_mark_for_line(row);
            assert_eq!(
                dm.gutter_mark_for_line(row),
                mark,
                "stored and live at row {row}"
            );
            mark
        })
        .collect()
    }

    /// The mark of a row in a hunk the tree pass never reached, or in a staged
    /// mark.
    fn unrefined(status: DiffHunkStatus, state: MarkState) -> GutterMark {
        GutterMark {
            status,
            tree: TreeMark::Unrefined,
            state,
        }
    }

    /// A deletion's mark holds no base line, so it answers at the one line it
    /// sits at, while a changed block answers across its lines.
    #[test]
    fn staged_marks_answer_by_base_line() {
        let dm = staged_map(
            "b0\nb1\nb2\nb3\nb4\n",
            Vec::new(),
            vec![
                mark(DiffHunkStatus::Deleted, 3..3),
                mark(DiffHunkStatus::Modified, 1..3),
            ],
        );
        assert_eq!(
            (0..5)
                .map(|line| dm.staged_mark_at_base_line(line))
                .collect::<Vec<_>>(),
            [
                None,
                Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged)),
                Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged)),
                Some(unrefined(DiffHunkStatus::Deleted, MarkState::Staged)),
                None,
            ],
            "marks set out of order answer by base line",
        );
    }

    /// A staged mark is a change already in the index, so the status bar
    /// counts it with the staged hunks.
    #[test]
    fn staged_marks_count_as_staged_hunks() {
        let mut dm = DiffMap::from_hunks(
            [added_hunk(4..5)],
            Some(Arc::new("b0\nb1\nb2\nb3\n".to_string())),
        );
        assert_eq!(dm.staged_counts(), (0, 1), "the unstaged hunk alone");

        dm.set_staged_marks(vec![
            mark(DiffHunkStatus::Modified, 0..1),
            mark(DiffHunkStatus::Deleted, 2..2),
        ]);
        assert_eq!(dm.staged_counts(), (2, 1), "each mark counts as staged");

        dm.set_staged_marks(vec![mark(DiffHunkStatus::Added, 3..4)]);
        assert_eq!(
            dm.staged_counts(),
            (1, 1),
            "a second set replaces the first"
        );
    }

    /// Two rows added above base line 1 push every later base line two rows
    /// down, so a mark on base line 3 paints on row 5.
    #[test]
    fn a_staged_mark_maps_past_an_added_hunk() {
        let added = DiffHunk {
            base_byte_range: 3..3,
            ..added_hunk(1..3)
        };
        let dm = staged_map(
            "b0\nb1\nb2\nb3\nb4\n",
            vec![added],
            vec![mark(DiffHunkStatus::Modified, 3..4)],
        );
        let text = "b0\nn1\nn2\nb1\nb2\nb3\nb4\n";

        let unstaged_add = Some(unrefined(DiffHunkStatus::Added, MarkState::Unstaged));
        assert_eq!(
            staged_gutter(&dm, text, 0..7),
            [
                None,
                unstaged_add,
                unstaged_add,
                None,
                None,
                Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged)),
                None,
            ],
            "the mark lands two rows below its base line",
        );
        assert_eq!(
            (0..7)
                .map(|row| dm.staged_for_line(row))
                .collect::<Vec<_>>(),
            [
                None,
                Some(MarkState::Unstaged),
                Some(MarkState::Unstaged),
                None,
                None,
                Some(MarkState::Staged),
                None
            ],
            "and reads staged there",
        );
    }

    /// Rows an unstaged hunk covers differ from the index, so the hunk's own
    /// mark paints there and the staged mark keeps only the rows past it.
    #[test]
    fn a_staged_mark_under_an_unstaged_hunk_yields_to_it() {
        let dm = staged_map(
            "b0\nb1\nb2\nb3\n",
            vec![modified_hunk(1..2, 3..5)],
            vec![mark(DiffHunkStatus::Modified, 1..3)],
        );
        let text = "b0\nX1\nb2\nb3\n";
        assert_eq!(
            staged_gutter(&dm, text, 0..4),
            [
                None,
                Some(unrefined(DiffHunkStatus::Modified, MarkState::Unstaged)),
                Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged)),
                None,
            ],
            "the hunk holds row 1 and the mark keeps row 2",
        );
        assert_eq!(
            dm.live_hunks(&buffer_holding(text).snapshot())
                .change_stops(),
            [1..2, 2..3],
            "and the walk stops on the hunk, then on the rest of the mark",
        );
    }

    /// Only a line diff makes a staged mark, so its rows read `Unrefined` while
    /// the hunk beside it reads the tree pass. A base line the hunk replaced
    /// takes the hunk's reading.
    #[test]
    fn a_staged_mark_reads_unrefined_beside_a_reached_hunk() {
        let hunk = DiffHunk {
            token_detail: Some(empty_detail(true)),
            marked_rows: std::iter::once(1..2).collect(),
            ..modified_hunk(1..2, 3..5)
        };
        let dm = staged_map(
            "b0\nb1\nb2\nb3\n",
            vec![hunk],
            vec![mark(DiffHunkStatus::Modified, 3..4)],
        );
        let staged = Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged));
        assert_eq!(
            (
                staged_gutter(&dm, "b0\nX1\nb2\nb3\n", 0..4),
                dm.base_line_mark(1),
                dm.staged_mark_at_base_line(3),
            ),
            (
                vec![None, modified(TreeMark::Marked), None, staged],
                Some((MarkState::Unstaged, TreeMark::Marked)),
                staged,
            ),
        );
    }

    /// A second read of one buffer version comes from the rows the first read
    /// cached, and it keeps the map's mark kind as the first read does.
    #[test]
    fn a_cached_resolve_keeps_the_mark_kind() {
        let dm = staged_map(
            "b0\nb1\nb2\nb3\n",
            vec![modified_hunk(1..2, 3..5)],
            vec![mark(DiffHunkStatus::Modified, 1..3)],
        )
        .with_mark_kind(MarkKind::Approved);
        let multi = buffer_holding("b0\nX1\nb2\nb3\n");
        let snapshot = multi.snapshot();
        let read = || {
            let live = dm.live_hunks(&snapshot);
            [1, 2].map(|row| live.gutter_mark_for_line(row))
        };
        let approval = [
            Some(unrefined(DiffHunkStatus::Modified, MarkState::Unapproved)),
            Some(unrefined(DiffHunkStatus::Modified, MarkState::Approved)),
        ];

        assert_eq!(
            [read(), read()],
            [approval, approval],
            "the cached read answers as the fresh one",
        );
    }

    /// A staged deletion took its lines out of the index, so it covers no row
    /// and marks the row after the removed text.
    #[test]
    fn a_staged_deletion_maps_to_a_seam() {
        let text = "b0\nb1\nb2\nb3\n";
        let dm = staged_map(text, Vec::new(), vec![mark(DiffHunkStatus::Deleted, 2..2)]);
        assert_eq!(
            staged_gutter(&dm, text, 0..4),
            [
                None,
                None,
                Some(unrefined(DiffHunkStatus::Deleted, MarkState::Staged)),
                None
            ],
            "the deletion marks row 2",
        );
        assert_eq!(dm.staged_for_line(2), None, "and covers no row to stage");
    }

    /// The status bar's position reports on the diff view's walk, which passes
    /// a staged mark. The mark counts in neither the total nor the position.
    #[test]
    fn staged_marks_stay_out_of_the_hunk_position() {
        let dm = staged_map(
            "b0\nb1\nb2\nb3\n",
            vec![modified_hunk(3..4, 9..12)],
            vec![mark(DiffHunkStatus::Modified, 1..2)],
        );
        assert_eq!(dm.hunk_count(), 1, "the hunk alone");

        let snapshot = buffer_holding("b0\nS1\nb2\nX3\n").snapshot();
        let live = dm.live_hunks(&snapshot);
        assert_eq!(
            (0..4)
                .map(|row| live.hunk_index_at(row))
                .collect::<Vec<_>>(),
            [None, None, None, Some(1)],
            "the mark on row 1 is no position, and the hunk on row 3 is the first",
        );
    }

    /// A plain pane's walk stops on a staged mark the way it stops on a hunk,
    /// and the diff view's walk passes it.
    #[test]
    fn staged_marks_are_change_stops_and_not_hunk_stops() {
        let dm = staged_map(
            "b0\nb1\nb2\nb3\nb4\n",
            vec![modified_hunk(1..2, 3..5)],
            vec![mark(DiffHunkStatus::Modified, 4..5)],
        );
        let live = dm.live_hunks(&buffer_holding("b0\nX1\nb2\nb3\nb4\n").snapshot());
        let rows = |stops: Vec<std::ops::Range<u32>>| {
            stops
                .into_iter()
                .map(|run| (run.start, run.end))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            (rows(live.hunk_stops()), rows(live.change_stops())),
            (vec![(1, 2)], vec![(1, 2), (4, 5)]),
        );
    }

    /// A row between two hunks answers the one before it rather than the one
    /// ahead, which is what keeps the status bar's position from blanking out
    /// as the reader walks the gap between them.
    #[test]
    fn the_hunk_at_a_row_is_the_one_containing_or_before_it() {
        let dm = DiffMap::from_hunks([added_hunk(2..4), added_hunk(8..9)], None);
        assert_eq!(dm.hunk_count(), 2, "both hunks counted");

        let snapshot = buffer_holding(&"x\n".repeat(10)).snapshot();
        let live = dm.live_hunks(&snapshot);
        let at = |row| live.hunk_index_at(row);
        assert_eq!(at(0), None, "above the first hunk nothing is behind it");
        assert_eq!(at(1), None, "and still nothing on the row before it");
        assert_eq!(at(2), Some(1), "the first hunk's own start");
        assert_eq!(at(3), Some(1), "and its later rows");
        assert_eq!(at(5), Some(1), "the gap answers the hunk behind it");
        assert_eq!(at(8), Some(2), "the second hunk's start steps the count");
        assert_eq!(at(400), Some(2), "and every row past it stays there");

        assert_eq!(
            DiffMap::from_hunks([], None)
                .live_hunks(&snapshot)
                .hunk_index_at(0),
            None,
            "an empty map has no position to report"
        );
    }

    /// The reader types while the next diff runs. The position counts a hunk
    /// from the row its text holds now, which is the row the walk lands on.
    #[test]
    fn an_anchored_hunk_answers_its_live_row() {
        let shared = Arc::new(std::sync::RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "l0\nl1\nl2\nl3\nl4\n",
        )));
        let mut dm = DiffMap::from_hunks([added_hunk(3..4)], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);
        shared.write().expect("poisoned").edit(0..3, "");

        let snapshot = MultiBuffer::singleton(shared).snapshot();
        let live = dm.live_hunks(&snapshot);
        assert_eq!(
            (0..4)
                .map(|row| live.hunk_index_at(row))
                .collect::<Vec<_>>(),
            [None, None, Some(1), Some(1)],
            "the row deleted above moves the hunk from row 3 to row 2",
        );
    }

    /// A removal of a file's last lines, in a file with no trailing newline,
    /// has no row after it. The walk lands it on the last row, so the position
    /// counts it from there.
    #[test]
    fn a_seam_past_the_last_row_counts_from_the_last_row() {
        let dm = DiffMap::from_hunks([deleted_hunk(2, 0..0)], None);
        let snapshot = buffer_holding("a\nb\nc").snapshot();
        let live = dm.live_hunks(&snapshot);
        assert_eq!(
            (0..3)
                .map(|row| live.hunk_index_at(row))
                .collect::<Vec<_>>(),
            [None, None, Some(1)],
        );
    }

    /// A removal inside the file counts from the live row above its seam, where
    /// the diff view's walk lands it, so the position names the removal as soon
    /// as the walk arrives.
    #[test]
    fn a_seam_inside_the_file_counts_from_the_row_above_it() {
        let dm = DiffMap::from_hunks([deleted_hunk(1, 0..0)], None);
        let snapshot = buffer_holding("a\nb\nc\nd").snapshot();
        let live = dm.live_hunks(&snapshot);
        assert_eq!(
            (0..4)
                .map(|row| live.hunk_index_at(row))
                .collect::<Vec<_>>(),
            [None, Some(1), Some(1), Some(1)],
        );
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
            Some(unrefined(DiffHunkStatus::Added, MarkState::Staged)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(2),
            Some(unrefined(DiffHunkStatus::Added, MarkState::Staged)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(3),
            None,
            "a row past the added range is unmarked",
        );
        assert_eq!(
            dm.gutter_mark_for_line(5),
            Some(unrefined(DiffHunkStatus::Modified, MarkState::Unstaged)),
        );
        assert_eq!(
            dm.gutter_mark_for_line(9),
            Some(unrefined(DiffHunkStatus::Deleted, MarkState::Staged)),
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
            Some(unrefined(DiffHunkStatus::Moved, MarkState::Unstaged)),
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
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..5,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 8..27,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: Some(vec![brave.clone()]),
                prose: false,
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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
                prose: ProseChange::None,
            }],
            "buffer spans narrow to the inserted word"
        );
        assert_eq!(
            td.base_spans,
            vec![ChangeSpan {
                byte_range: 8..21,
                kind: ChangeKind::Replaced,
                move_metadata: None,
                prose: ProseChange::None,
            }],
            "base spans fall back to the whole replaced literal"
        );
        assert_eq!(&rhs_text[brave], "brave ");
    }

    /// A literal that kept some of its words is a part. A comment whose every
    /// word changed is whole, though its marker sits outside the changed chars.
    #[test]
    fn a_part_of_a_literal_and_a_whole_literal_report_their_prose_state() {
        use stoat_language::structural_diff::{
            ChangeKind as LangChangeKind, DiffChange, DiffResult, Side,
        };
        let lhs_text = "let s = \"one aaa three\";\n// aaa bbb\n";
        let rhs_text = "let s = \"one zzz three\";\n// zzz yyy\n";
        let at = |text: &str, needle: &str| {
            let start = text.find(needle).expect("needle in text");
            start..start + needle.len()
        };
        let pair = |id: u32, side: Side, text: &str, literal: &str, changed: &str| DiffChange {
            side,
            byte_range: at(text, literal),
            kind: LangChangeKind::Replaced,
            move_metadata: None,
            pair_id: Some(id),
            deletion_rhs_anchor: None,
            refined_spans: Some(vec![at(text, changed)]),
            prose: true,
        };
        let changes = vec![
            pair(0, Side::Lhs, lhs_text, "one aaa three", "aaa"),
            pair(0, Side::Rhs, rhs_text, "one zzz three", "zzz"),
            pair(1, Side::Lhs, lhs_text, "// aaa bbb", "aaa bbb"),
            pair(1, Side::Rhs, rhs_text, "// zzz yyy", "zzz yyy"),
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
            },
            Arc::new(lhs_text.to_string()),
            rhs_text,
        );

        let states = |spans: &[ChangeSpan]| spans.iter().map(|s| s.prose).collect::<Vec<_>>();
        let prose = |line: u32| {
            let td = dm
                .token_detail_for_line(line)
                .expect("modified hunk carries token detail");
            (states(&td.buffer_spans), states(&td.base_spans))
        };
        assert_eq!(
            (prose(0), prose(1)),
            (
                (vec![ProseChange::Part], vec![ProseChange::Part]),
                (vec![ProseChange::Whole], vec![ProseChange::Whole]),
            ),
            "the string keeps words around its edit, the comment keeps only its marker"
        );
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
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..10,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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
                    .map(move |(r, kind, _)| (line, r.start, r.end, kind.clone()))
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
    fn a_base_line_is_refined_only_under_a_modified_hunk_with_detail() {
        // Base line 1 is the hunk's line, and base line 0 is one no hunk covers.
        let refined = |status: DiffHunkStatus, token_detail: Option<Arc<TokenDetail>>| {
            let rows = match status {
                DiffHunkStatus::Deleted => 1..1,
                _ => 1..2,
            };
            let mut dm = DiffMap::default();
            dm.set_base_text(Arc::new("keep\nold\n".to_string()));
            dm.push_hunk(DiffHunk {
                status,
                buffer_start_line: rows.start,
                buffer_line_range: rows,
                base_byte_range: 5..9,
                anchor_range: None,
                token_detail,
                unstaged_lines: Vec::new(),
                marked_rows: Vec::new(),
            });
            [dm.base_line_refined(1), dm.base_line_refined(0)]
        };
        let empty = TokenDetail {
            buffer_spans: Vec::new(),
            base_spans: Vec::new(),
            tree: false,
        };

        assert_eq!(
            [
                refined(DiffHunkStatus::Modified, Some(Arc::new(empty))),
                refined(DiffHunkStatus::Modified, None),
                refined(DiffHunkStatus::Deleted, None),
            ],
            [[true, false], [false, false], [false, false]],
            "only a modified hunk with detail refines its base line",
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
            refined_spans: None,
            prose: false,
        }];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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
            refined_spans: None,
            prose: false,
        }];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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
        assert!(dm.deleted_blocks(true, None, &|_| 1).is_empty());
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
        assert!(dm.deleted_blocks(true, None, &|_| 1).is_empty());
    }

    #[test]
    fn single_deleted_hunk() {
        let base = "deleted line\n";
        let dm = DiffMap::from_hunks([deleted_hunk(2, 0..13)], Some(Arc::new(base.to_string())));

        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
        assert!(dm.has_deletion_after(2, true));
        assert!(!dm.has_deletion_after(1, true));
        assert!(!dm.has_deletion_after(3, true));

        let blocks = dm.deleted_blocks(true, None, &|_| 1).concat();
        assert_eq!(blocks.len(), 1);
        assert_eq!(block_rows(&blocks[0]), ["deleted line"]);
        assert_eq!(dm.total_deleted_lines(), 1);
    }

    /// The rows `block` paints, as text.
    fn block_rows(block: &BlockProperties) -> Vec<String> {
        let ctx = crate::display_map::BlockContext {
            block_id: crate::display_map::BlockId::Custom(crate::display_map::CustomBlockId(0)),
            max_width: 80,
            height: block.height.unwrap_or(0),
            selected: false,
            anchor_row: 0,
            diff_status: None,
            buffer_snapshot: &crate::multi_buffer::MultiBufferSnapshot::empty(),
        };
        (block.render)(&ctx)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A paired base line's segments past its live line's rows block below
    /// that live row, carrying on the line from the segment the live line
    /// stopped at.
    #[test]
    fn a_paired_base_line_overflows_below_its_live_row() {
        let base = "alpha beta gamma delta\n";
        let dm = DiffMap::from_hunks(
            [modified_hunk(3..4, 0..base.len())],
            Some(Arc::new(base.to_string())),
        );
        let wrap = WrapGeometry {
            width: 12,
            tab_size: 4,
            max_expansion_column: u32::MAX,
        };
        let blocks = |live_rows: u32| {
            dm.deleted_blocks(true, Some(wrap), &|_| live_rows)
                .concat()
                .iter()
                .map(|block| (block.placement, block.first_segment, block_rows(block)))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            blocks(1),
            [(BlockPlacement::Below(3), 1, vec!["gamma delta".to_string()])],
        );
        assert_eq!(
            blocks(2),
            [],
            "a live line with a row per segment leaves nothing to overflow"
        );
    }

    /// A deleted line wider than the wrap width takes a row per segment, and
    /// each base line is a block of its own, so a block's first row is its
    /// line's first row.
    #[test]
    fn a_wrapped_deletion_blocks_each_line_by_itself() {
        let base = "alpha beta gamma delta\nshort\n";
        let dm = DiffMap::from_hunks(
            [deleted_hunk(2, 0..base.len())],
            Some(Arc::new(base.to_string())),
        );
        let wrap = WrapGeometry {
            width: 12,
            tab_size: 4,
            max_expansion_column: u32::MAX,
        };

        let groups = dm.deleted_blocks(true, Some(wrap), &|_| 1);
        assert_eq!(
            groups
                .iter()
                .map(|group| group.iter().map(block_rows).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            [vec![vec!["alpha beta ", "gamma delta"], vec!["short"]]],
        );
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
        assert_eq!(dm.deleted_blocks(true, None, &|_| 1).len(), 0);
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

        let groups = dm.deleted_blocks(true, None, &|_| 1);
        assert_eq!(
            groups
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .map(|block| (block.placement, block.height))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
            [vec![
                (BlockPlacement::Below(3), Some(1)),
                (BlockPlacement::Below(3), Some(1)),
            ]],
            "the two base rows past the live one block, a row each, and hang off \
             the hunk's last live row",
        );

        assert!(dm.has_deletion_after(3, true), "the block sits below row 3");
        assert!(!dm.has_deletion_after(2, true), "not above the hunk");
    }

    /// In the unified layout a fold over live rows hides the base lines they
    /// index-pair with. Each run of shown rows blocks its lines above its first
    /// row, and the excess follows the last live row, below it when a fold
    /// hides it.
    #[test]
    fn a_fold_in_the_unified_layout_blocks_each_shown_run_above_itself() {
        let dm = DiffMap::from_hunks(
            [modified_hunk(2..8, 0..14)],
            Some(Arc::new("a\nb\nc\nd\ne\nf\ng\n".to_string())),
        );
        let placements = |hidden: std::ops::Range<u32>| {
            dm.deleted_blocks(false, None, &|row| match hidden.contains(&row) {
                true => u32::MAX,
                false => 1,
            })
            .into_iter()
            .flatten()
            .map(|block| block.placement)
            .collect::<Vec<_>>()
        };
        let below = BlockPlacement::Below;
        assert_eq!(
            [placements(4..6), placements(6..8)],
            [
                vec![below(1), below(1), below(5), below(5), below(5)],
                vec![below(1), below(1), below(1), below(1), below(7)],
            ],
            "rows 2 and 3 take a and b, rows 6 and 7 take e and f with the excess g; \
             then rows 2 to 5 take a to d, and g hangs below the hidden last row",
        );
    }

    /// The count the diff view maps a viewport top through. A row is
    /// base-present while its hunk's base rows reach it, so the rows of an
    /// added hunk and of a moved-to hunk all count, and a modified hunk's only
    /// past its paired prefix.
    #[test]
    fn rows_without_base_counts_added_moved_and_unpaired_modified_rows() {
        let base = "a\n";
        let moved_to = DiffHunk {
            status: DiffHunkStatus::Moved,
            ..added_hunk(3..5)
        };
        let dm = DiffMap::from_hunks(
            [added_hunk(1..3), moved_to, modified_hunk(5..8, 0..2)],
            Some(Arc::new(base.to_string())),
        );

        assert_eq!(
            [3, 6, 7, 9].map(|row| dm.rows_without_base_before(row, true)),
            [2, 4, 5, 6],
            "the added hunk's two rows, then the moved-to hunk's two, then the \
             modified hunk's rows past its one paired row"
        );
    }

    /// A seam counts the base lines it removed once its own row is in the
    /// range, since those lines come before that row.
    #[test]
    fn seam_base_lines_in_counts_moved_away_lines() {
        let seam = DiffHunk {
            status: DiffHunkStatus::Moved,
            base_byte_range: 2..3,
            ..added_hunk(1..1)
        };
        let dm = DiffMap::from_hunks([seam], Some(Arc::new("a\nb\nc\nd\n".to_string())));

        assert_eq!(
            (dm.seam_base_lines_in(0..1), dm.seam_base_lines_in(0..2)),
            (0, 1),
            "the seam at row 1 adds its one line once row 1 is in the range"
        );
    }

    /// The count covers each line in the ranges the line differ makes, which
    /// end before their last line's newline: one line, two lines, a lone blank
    /// line as an empty range, a run that ends in a blank line, a CRLF line, a
    /// tail with no newline, and the whole text.
    #[test]
    fn base_line_count_counts_each_line_the_line_differ_names() {
        let base = "a\nbb\n\nccc\r\nd";
        let dm = DiffMap::from_hunks(
            [modified_hunk(0..1, 0..1)],
            Some(Arc::new(base.to_string())),
        );
        let counts: Vec<u32> = [0..1, 0..4, 5..5, 2..5, 6..10, 11..12, 0..12]
            .into_iter()
            .map(|range| dm.base_line_count(&modified_hunk(0..1, range)))
            .collect();
        assert_eq!(counts, [1, 2, 1, 2, 1, 1, 5]);

        let mut set = DiffMap::from_hunks([modified_hunk(0..1, 0..1)], None);
        set.set_base_text(Arc::new(base.to_string()));
        assert_eq!(
            set.base_line_count(&modified_hunk(0..1, 0..4)),
            2,
            "a base set after construction carries its line table",
        );
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
        assert_eq!(dm.deleted_blocks(true, None, &|_| 1).len(), 1);
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
                prose: ProseChange::None,
            }],
            base_spans: vec![],
            tree: false,
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
    fn an_unsearched_hunk_keeps_the_line_pass_detail() {
        let base = "one\ntwo\nthree\nfour\nfive\n";
        let buffer = "ONE\ntwo\nthree\nfour\nFIVE\n";
        let novel = |side, byte_range| DiffChange {
            side,
            byte_range,
            kind: LangChangeKind::Novel,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
            refined_spans: None,
            prose: false,
        };
        let tree = [
            novel(Side::Lhs, 0..3),
            novel(Side::Lhs, 19..23),
            novel(Side::Rhs, 0..3),
            novel(Side::Rhs, 19..23),
        ];

        let span = ChangeSpan {
            byte_range: 19..23,
            kind: ChangeKind::Novel,
            move_metadata: None,
            prose: ProseChange::None,
        };
        let tree_detail = Arc::new(TokenDetail {
            buffer_spans: vec![span.clone()],
            base_spans: vec![span],
            tree: true,
        });

        // A span on either side alone keeps the first hunk's detail. The empty
        // range past the text is the side that reaches nothing.
        for unsearched in [[(0..3, 0..3)], [(0..3, 30..30)], [(30..30, 0..3)]] {
            let mut hunks = super::changes_to_hunks(
                &stoat_language::structural_diff::diff(base, buffer).changes,
                base,
                buffer,
            );
            let line_pass = hunks[0].token_detail.clone();

            super::merge_structural_detail(&mut hunks, &tree, &unsearched, base, buffer);

            assert_eq!(
                hunks
                    .iter()
                    .map(|hunk| hunk.token_detail.clone())
                    .collect::<Vec<_>>(),
                [line_pass, Some(tree_detail.clone())],
                "with {unsearched:?} unsearched, the first hunk keeps the line pass's detail",
            );
        }
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

    /// Marks for every row of `hunks`' combined range, `None` where no hunk
    /// marks.
    fn marks_over(
        hunks: Vec<DiffHunk>,
        rows: std::ops::Range<u32>,
        text: &str,
    ) -> Vec<Option<GutterMark>> {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        let map = DiffMap::from_hunks(hunks, Some(Arc::new(text.to_string())));
        let tb = TextBuffer::with_text(BufferId::new(0), text);
        let multi = MultiBuffer::singleton(Arc::new(std::sync::RwLock::new(tb)));
        let snapshot = multi.snapshot();
        let live = map.live_hunks(&snapshot);
        rows.map(|row| live.gutter_mark_for_line(row)).collect()
    }

    /// A hunk over `rows`, which the tree pass refined to `marked` when
    /// `marked` is non-empty and never reached otherwise.
    fn refined_hunk(rows: std::ops::Range<u32>, marked: Vec<std::ops::Range<u32>>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: rows.start,
            buffer_line_range: rows.clone(),
            base_byte_range: 0..1,
            anchor_range: None,
            token_detail: (!marked.is_empty()).then(|| empty_detail(true)),
            unstaged_lines: std::iter::once(rows).collect(),
            marked_rows: marked,
        }
    }

    /// A detail with no spans, written by the tree pass when `tree` is set.
    fn empty_detail(tree: bool) -> Arc<TokenDetail> {
        Arc::new(TokenDetail {
            buffer_spans: Vec::new(),
            base_spans: Vec::new(),
            tree,
        })
    }

    /// The mark of an unstaged row in a `Modified` hunk the tree pass reads as
    /// `tree`.
    fn modified(tree: TreeMark) -> Option<GutterMark> {
        Some(GutterMark {
            status: DiffHunkStatus::Modified,
            tree,
            state: MarkState::Unstaged,
        })
    }

    /// The stops a map offers over `text`.
    fn stops_of(hunks: Vec<DiffHunk>, text: &str) -> Vec<std::ops::Range<u32>> {
        use crate::{
            buffer::{BufferId, TextBuffer},
            multi_buffer::MultiBuffer,
        };
        let map = DiffMap::from_hunks(hunks, Some(Arc::new(text.to_string())));
        let tb = TextBuffer::with_text(BufferId::new(0), text);
        let multi = MultiBuffer::singleton(Arc::new(std::sync::RwLock::new(tb)));
        map.live_hunks(&multi.snapshot()).change_stops()
    }

    /// Navigation stops on what changed. One refined hunk over a block offers a
    /// stop per run, so a walk crosses the changed rows rather than stepping
    /// over the whole block in one press.
    ///
    /// The stops tile the hunk. A row between two runs joins the run above it,
    /// and a row above the first run joins that run.
    #[test]
    fn a_refined_hunk_offers_a_stop_per_marked_run() {
        let text = "0\n1\n2\n3\n4\n5\n";
        assert_eq!(
            stops_of(vec![refined_hunk(0..6, vec![1..2, 4..5])], text),
            [0..4, 4..6],
            "each run's stop reaches the next run"
        );
    }

    /// Deleting the rows under a refined hunk's second run leaves that run
    /// past the hunk's live end. That run opens no stop, since the walk lands
    /// an empty stop as a removal, on the row below the hunk.
    #[test]
    fn a_run_past_a_shrunk_hunk_opens_no_stop() {
        let multi = buffer_holding("0\n1\n2\n3\n4\n5\n6\n");
        let shared = multi.as_singleton().expect("singleton").clone();
        let mut dm = DiffMap::from_hunks([refined_hunk(0..6, vec![1..2, 4..5])], None);
        dm.anchor_hunks(&shared.read().expect("poisoned").snapshot);

        shared.write().expect("poisoned").edit(6..12, "");

        assert_eq!(
            dm.live_hunks(&multi.snapshot()).hunk_stops(),
            std::iter::once(0..3).collect::<Vec<_>>(),
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

    /// Stops come back in document order across hunks and their runs, so a
    /// walk never doubles back. A hunk with one run is one stop over all of it.
    #[test]
    fn stops_arrive_in_document_order() {
        let text = "0\n1\n2\n3\n4\n5\n6\n7\n";
        let hunks = vec![
            refined_hunk(0..3, vec![0..1, 2..3]),
            refined_hunk(4..7, std::iter::once(5..6).collect()),
        ];
        assert_eq!(
            stops_of(hunks, text),
            [0..2, 2..3, 4..7],
            "stops from both hunks follow row order"
        );
    }

    /// The stops, the change position at `row`, and the change count a map of
    /// `hunks` offers over `text`, all read from one live answer.
    fn live_answer(
        hunks: Vec<DiffHunk>,
        text: &str,
        row: u32,
    ) -> (Vec<std::ops::Range<u32>>, Option<usize>, usize) {
        let map = DiffMap::from_hunks(hunks, Some(Arc::new(text.to_string())));
        let multi = buffer_holding(text);
        let snapshot = multi.snapshot();
        let live = map.live_hunks(&snapshot);
        (
            live.hunk_stops(),
            live.hunk_index_at(row),
            live.change_count(),
        )
    }

    /// Two hunks with only a blank row between them are one change. The walk
    /// stops once over both, the position counts one, and the blank row takes
    /// the change's mark. The blank row lies in no hunk, so the tree pass never
    /// marks it, and it reads `Unmarked`.
    #[test]
    fn hunks_a_blank_row_apart_are_one_change() {
        let text = "0\n1\n\n3\n4\n";
        let hunks = || {
            vec![
                refined_hunk(1..2, Vec::new()),
                refined_hunk(3..4, Vec::new()),
            ]
        };
        let unreached = modified(TreeMark::Unrefined);
        assert_eq!(
            (
                live_answer(hunks(), text, 3),
                marks_over(hunks(), 0..5, text)
            ),
            (
                (std::iter::once(1..4).collect(), Some(1), 1),
                vec![
                    None,
                    unreached,
                    modified(TreeMark::Unmarked),
                    unreached,
                    None
                ],
            ),
            "one stop, position one of one, and the blank row takes the change's mark",
        );
    }

    /// A row that holds text between two hunks keeps them two changes.
    #[test]
    fn hunks_a_text_row_apart_stay_two_changes() {
        let text = "0\n1\n2\n3\n4\n";
        let hunks = vec![
            refined_hunk(1..2, Vec::new()),
            refined_hunk(3..4, Vec::new()),
        ];
        assert_eq!(
            live_answer(hunks, text, 3),
            (vec![1..2, 3..4], Some(2), 2),
            "two stops, and the second hunk at position two of two",
        );
    }

    /// The gap starts at the upper hunk's last marked row, so a brace row the
    /// hunk holds but does not mark keeps the two hunks apart.
    #[test]
    fn an_unmarked_brace_row_keeps_two_hunks_apart() {
        let text = "a\n}\n\n\nc\n";
        let hunks = vec![
            refined_hunk(0..3, std::iter::once(0..1).collect()),
            refined_hunk(4..5, Vec::new()),
        ];
        assert_eq!(
            live_answer(hunks, text, 4),
            (vec![0..3, 4..5], Some(2), 2),
            "the brace row splits the two hunks",
        );
    }

    /// A gap that reaches into the hunks marks too. The rows past the upper
    /// hunk's last run and before the lower hunk's first run belong to the one
    /// change, which the tree pass found on the runs alone.
    #[test]
    fn refined_hunks_a_blank_gap_apart_mark_across_it() {
        let text = "a\n\n\n\nb\n";
        let hunks = || {
            vec![
                refined_hunk(0..2, std::iter::once(0..1).collect()),
                refined_hunk(3..5, std::iter::once(4..5).collect()),
            ]
        };
        let (marked, unmarked) = (modified(TreeMark::Marked), modified(TreeMark::Unmarked));
        assert_eq!(
            (
                live_answer(hunks(), text, 4),
                marks_over(hunks(), 0..5, text)
            ),
            (
                (std::iter::once(0..5).collect(), Some(1), 1),
                vec![marked, unmarked, unmarked, unmarked, marked],
            ),
            "one stop, one change, and every row of the gap takes the change's mark",
        );
    }
    /// A hunk whose real change is two tokens marks two rows for the tree pass,
    /// not the hundred its extents cover. Every row keeps the line pass's mark.
    #[test]
    fn a_refined_hunk_marks_only_the_rows_its_spans_touch() {
        let text = "0\n1\n2\n3\n4\n5\n";
        let hunk = refined_hunk(0..6, vec![1..2, 4..5]);
        let (marked, unmarked) = (modified(TreeMark::Marked), modified(TreeMark::Unmarked));

        assert_eq!(
            marks_over(vec![hunk], 0..6, text),
            [unmarked, marked, unmarked, unmarked, marked, unmarked],
            "only the marked runs read marked"
        );
    }

    /// The tree pass reads a row it found a change on as `Marked`, and the
    /// hunk's other row as `Unmarked`.
    #[test]
    fn a_reached_hunk_reads_marked_and_unmarked_rows() {
        assert_eq!(
            reindent_marks(true),
            [
                None,
                modified(TreeMark::Marked),
                modified(TreeMark::Unmarked),
                None
            ],
        );
    }

    /// A detail the line pass wrote leaves every row of the hunk `Unrefined`,
    /// whatever runs the hunk holds.
    #[test]
    fn a_hunk_the_tree_pass_never_reached_reads_unrefined() {
        let unreached = modified(TreeMark::Unrefined);
        assert_eq!(reindent_marks(false), [None, unreached, unreached, None]);
    }

    /// The line pass reports no move. A moved line's deletion and addition read
    /// `Unrefined` until the tree pass finds the line relocated, and then both
    /// read as a `Marked` move.
    #[test]
    fn a_moved_line_reads_marked_once_the_tree_pass_lands() {
        let (base, buffer) = ("a\nb\nc\nd\n", "a\nc\nd\nb\n");
        let moved = |side, byte_range| DiffChange {
            side,
            byte_range,
            kind: LangChangeKind::Moved,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
            refined_spans: None,
            prose: false,
        };
        let mut hunks = super::changes_to_hunks(
            &stoat_language::structural_diff::diff(base, buffer).changes,
            base,
            buffer,
        );
        let line_pass = marks_over(hunks.clone(), 0..5, buffer);
        super::merge_structural_detail(
            &mut hunks,
            &[moved(Side::Lhs, 2..3), moved(Side::Rhs, 6..7)],
            &[],
            base,
            buffer,
        );
        let mark = |status, tree| {
            Some(GutterMark {
                status,
                tree,
                state: MarkState::Unstaged,
            })
        };

        assert_eq!(
            [line_pass, marks_over(hunks, 0..5, buffer)],
            [
                [
                    None,
                    mark(DiffHunkStatus::Deleted, TreeMark::Unrefined),
                    None,
                    mark(DiffHunkStatus::Added, TreeMark::Unrefined),
                    None,
                ],
                [
                    None,
                    mark(DiffHunkStatus::Moved, TreeMark::Marked),
                    None,
                    mark(DiffHunkStatus::Moved, TreeMark::Marked),
                    None,
                ],
            ],
            "the removal's seam on row 1 and the addition on row 3",
        );
    }

    /// The marks over a reindent of rows 1 and 2, as one `Modified` hunk with a
    /// run on row 1 alone and a detail whose tree flag is `tree`.
    fn reindent_marks(tree: bool) -> Vec<Option<GutterMark>> {
        let hunk = DiffHunk {
            token_detail: Some(empty_detail(tree)),
            ..refined_hunk(1..3, std::iter::once(1..2).collect())
        };
        marks_over(vec![hunk], 0..4, "a\n    b\n    c\nd\n")
    }

    /// The runs [`super::marked_row_runs`] makes of spans over `ranges` in
    /// `text`, each as its `(start, end)` rows.
    fn runs_over(text: &str, ranges: &[std::ops::Range<usize>]) -> Vec<(u32, u32)> {
        let starts = super::line_starts(text);
        let spans: Vec<ChangeSpan> = ranges
            .iter()
            .map(|range| ChangeSpan {
                byte_range: range.clone(),
                kind: ChangeKind::Replaced,
                move_metadata: None,
                prose: ProseChange::None,
            })
            .collect();
        super::marked_row_runs(
            &spans,
            |off| super::line_of(&starts, off),
            |row| super::line_is_blank(&starts, text, row),
        )
        .into_iter()
        .map(|run| (run.start, run.end))
        .collect()
    }

    /// A row holding only whitespace sits inside the change around it, so the
    /// runs on either side of it join. A row holding content keeps them apart.
    #[test]
    fn marked_runs_join_across_whitespace_only_rows() {
        assert_eq!(
            (
                runs_over("x\na\n\nb\ny\n", &[2..3, 5..6]),
                runs_over("x\na\nz\nb\ny\n", &[2..3, 6..7]),
                runs_over("a\n\n  \nb\n", &[0..1, 6..7]),
            ),
            (vec![(1, 4)], vec![(1, 2), (3, 4)], vec![(0, 4)]),
            "a blank row joins, a content row splits, two whitespace rows join",
        );
    }

    /// A blank row that only gains indentation puts the two statements around
    /// it in one line hunk, and the tree pass marks the row with them.
    #[test]
    fn a_whitespace_row_between_two_refined_statements_marks_with_them() {
        let base = "fn f() {\n    let a = 1;\n\n    let b = 2;\n}\n";
        let buffer = "fn f() {\n    let a = 10;\n    \n    let b = 20;\n}\n";
        let marked: Vec<Vec<std::ops::Range<u32>>> = rust_hunks(base, buffer)
            .into_iter()
            .map(|hunk| hunk.marked_rows)
            .collect();
        assert_eq!(
            marked,
            vec![vec![1..4]],
            "one hunk, one run from the first statement through the second",
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
                modified(TreeMark::Unrefined),
                modified(TreeMark::Unrefined),
                None,
            ],
            "every row of the range marks"
        );
    }

    /// An addition marks every row it added. Once the tree pass reaches it,
    /// every row reads `Marked`, a row its spans miss included.
    #[test]
    fn an_added_hunk_marks_its_whole_range() {
        let text = "0\n1\n2\n3\n";
        let marks = |token_detail, marked_rows| {
            let hunk = DiffHunk {
                status: DiffHunkStatus::Added,
                buffer_start_line: 1,
                buffer_line_range: 1..3,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail,
                unstaged_lines: std::iter::once(1..3).collect(),
                marked_rows,
            };
            marks_over(vec![hunk], 0..4, text)
        };
        let added = |tree| {
            Some(GutterMark {
                status: DiffHunkStatus::Added,
                tree,
                state: MarkState::Unstaged,
            })
        };

        assert_eq!(
            [
                marks(None, Vec::new()),
                marks(Some(empty_detail(true)), std::iter::once(1..2).collect()),
            ],
            [
                [
                    None,
                    added(TreeMark::Unrefined),
                    added(TreeMark::Unrefined),
                    None
                ],
                [None, added(TreeMark::Marked), added(TreeMark::Marked), None],
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
                tree: false,
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
                prose: ProseChange::None,
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

    #[test]
    fn an_added_blank_line_is_a_one_row_hunk() {
        let dm = DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff("a\nb\n", "a\n\nb\n"),
            Arc::new("a\nb\n".to_string()),
            "a\n\nb\n",
        );
        let hunks: Vec<(DiffHunkStatus, std::ops::Range<u32>)> = dm
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|h| (h.status, h.buffer_line_range.clone()))
            .collect();
        assert_eq!(hunks, [(DiffHunkStatus::Added, 1..2)]);
    }

    #[test]
    fn a_deleted_blank_line_counts_one_base_line() {
        let base_lines =
            |base: &str, buffer: &str| -> Vec<(DiffHunkStatus, std::ops::Range<u32>)> {
                let result = stoat_language::structural_diff::diff(base, buffer);
                let starts = super::line_starts(base);
                super::changes_to_hunks(&result.changes, base, buffer)
                    .iter()
                    .map(|hunk| (hunk.status, super::hunk_base_lines(hunk, &starts, base)))
                    .collect()
            };
        assert_eq!(
            base_lines("a\n\nb\n", "a\nb\n"),
            [(DiffHunkStatus::Deleted, 1..2)],
            "a lone blank line"
        );
        assert_eq!(
            base_lines("a\nb\n\nc\n", "a\nc\n"),
            [(DiffHunkStatus::Deleted, 1..3)],
            "a run that ends in a blank line"
        );
    }

    /// The line differ ends a removed run before its last line's newline, so a
    /// lone blank line is an empty range and a run that ends in one stops where
    /// that line starts. Each removed line still blocks a row of its own.
    #[test]
    fn a_removed_blank_line_at_a_runs_edge_blocks_a_row() {
        let removed = |base: &str, buffer: &str| {
            let dm = DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, buffer),
                Arc::new(base.to_string()),
                buffer,
            );
            let rows: Vec<Vec<String>> = dm
                .deleted_blocks(true, None, &|_| 1)
                .concat()
                .iter()
                .map(block_rows)
                .collect();
            (
                rows,
                dm.total_deleted_lines(),
                dm.has_deletion_after(0, true),
            )
        };

        assert_eq!(
            removed("a\n\nb\n", "a\nb\n"),
            (vec![vec![String::new()]], 1, true),
            "a lone blank line"
        );
        assert_eq!(
            removed("a\nb\n\nc\n", "a\nc\n"),
            (vec![vec!["b".to_string()], vec![String::new()]], 2, true),
            "a run that ends in a blank line"
        );
    }

    /// A replaced run that ends in a blank line pairs that line with a live row
    /// too, so one fewer live row has no base line beside it.
    #[test]
    fn a_replaced_run_that_ends_in_a_blank_line_pairs_it() {
        let (base, buffer) = ("a\nb\n\nc\n", "a\nX\nY\nZ\nc\n");
        let dm = DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(base, buffer),
            Arc::new(base.to_string()),
            buffer,
        );
        let hunks: Vec<(DiffHunkStatus, u32)> = dm
            .hunks()
            .map(|hunk| (hunk.status, dm.base_line_count(hunk)))
            .collect();

        assert_eq!(
            (hunks, dm.rows_without_base_before(5, true)),
            (vec![(DiffHunkStatus::Modified, 2)], 1),
        );
    }

    /// A whole-file removal's range runs past the final newline, and the empty
    /// tail there is no line of the base.
    #[test]
    fn a_whole_file_removal_counts_each_base_line_once() {
        let base = "a\nb\n";
        let starts = super::line_starts(base);
        assert_eq!(
            super::hunk_base_lines(&deleted_hunk(0, 0..4), &starts, base),
            0..2
        );
    }

    /// The tree pass turns an `Added` hunk it finds relocated into a `Moved`
    /// one, and that hunk keeps the empty base range it had as an addition.
    #[test]
    fn a_moved_hunk_with_an_empty_base_range_counts_no_base_line() {
        let base = "a\nb\n";
        let moved = DiffHunk {
            status: DiffHunkStatus::Moved,
            ..added_hunk(1..3)
        };
        assert_eq!(
            super::hunk_base_lines(&moved, &super::line_starts(base), base),
            0..0
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
            Some(MarkState::Staged),
            "line 1 matches the index, so it reads staged"
        );
        assert_eq!(
            dm.staged_for_line(2),
            Some(MarkState::Unstaged),
            "line 2 still differs from the index, so it reads unstaged"
        );
    }

    /// The search marks what a test of every range against every hunk marks.
    ///
    /// The cases cover a zero-width hunk at a range's end, zero-width ranges at
    /// a hunk's start and end, one range across two hunks, and ranges and a
    /// hunk that reach nothing.
    #[test]
    fn mark_staged_finds_every_overlap_a_full_scan_finds() {
        let index_changed = [0..1, 3..5, 8..8, 12..20, 25..25, 30..31];
        let lines = [5..5, 8..10, 12..14, 16..18, 22..25, 27..29];
        let mut hunks: Vec<DiffHunk> = lines.iter().cloned().map(added_hunk).collect();

        super::mark_staged(&mut hunks, &index_changed);

        let marked: Vec<Vec<std::ops::Range<u32>>> =
            hunks.into_iter().map(|hunk| hunk.unstaged_lines).collect();
        let full_scan: Vec<Vec<std::ops::Range<u32>>> = lines
            .iter()
            .map(|range| {
                if range.start == range.end {
                    match index_changed
                        .iter()
                        .any(|c| super::ranges_overlap(c, range))
                    {
                        true => vec![range.clone()],
                        false => Vec::new(),
                    }
                } else {
                    index_changed
                        .iter()
                        .filter(|c| super::ranges_overlap(c, range))
                        .map(|c| c.start.max(range.start)..c.end.min(range.end))
                        .collect()
                }
            })
            .collect();
        let one = |range: std::ops::Range<u32>| vec![range];
        let expected = vec![
            one(5..5),
            one(8..8),
            one(12..14),
            one(16..18),
            one(25..25),
            Vec::new(),
        ];
        assert_eq!(
            (marked, full_scan),
            (expected.clone(), expected),
            "the search marks what the full scan marks, and both read as written",
        );
    }

    #[test]
    fn partially_staged_hunk_reads_per_line() {
        let mut hunk = modified_hunk(1..3, 10..14);
        hunk.unstaged_lines = std::iter::once(2..3).collect();
        let dm = DiffMap::from_hunks([hunk], None);
        assert_eq!(
            dm.gutter_mark_for_line(1),
            Some(unrefined(DiffHunkStatus::Modified, MarkState::Staged)),
            "row one is staged within the hunk"
        );
        assert_eq!(
            dm.gutter_mark_for_line(2),
            Some(unrefined(DiffHunkStatus::Modified, MarkState::Unstaged)),
            "row two is still unstaged"
        );
        assert_eq!(dm.staged_for_line(1), Some(MarkState::Staged));
        assert_eq!(dm.staged_for_line(2), Some(MarkState::Unstaged));
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
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(rhs_meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
        ];
        let result = DiffResult {
            changes,
            fell_back_to_line_diff: false,
            matched: Vec::new(),
            unchanged_lines: Vec::new(),
            unsearched: Vec::new(),
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
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 32..51,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
                refined_spans: None,
                prose: false,
            },
        ];
        let dm = DiffMap::from_structural_changes(
            DiffResult {
                changes,
                fell_back_to_line_diff: false,
                matched: Vec::new(),
                unchanged_lines: Vec::new(),
                unsearched: Vec::new(),
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

    /// Each hunk takes the tree spans on its own lines and none of its
    /// neighbors', as the line pass found three hunks between unchanged lines.
    #[test]
    fn each_hunk_takes_only_the_tree_spans_inside_it() {
        let span = |at: usize| ChangeSpan {
            byte_range: at..at + 1,
            kind: ChangeKind::Novel,
            move_metadata: None,
            prose: ProseChange::None,
        };
        let detail = |at| {
            Some(Arc::new(TokenDetail {
                buffer_spans: vec![span(at)],
                base_spans: vec![span(at)],
                tree: true,
            }))
        };

        assert_eq!(
            rust_detail("a\nb\nc\nx\ny", "A\nb\nC\nx\nY"),
            [detail(0), detail(4), detail(8)],
            "each line's changed token lands in that line's hunk alone",
        );
    }

    /// A removed visibility modifier is no move, even when the same modifier
    /// appears on another line, so its base bytes keep their removal span.
    #[test]
    fn a_removed_visibility_modifier_marks_its_base_span_when_it_appears_elsewhere() {
        let base = "mod m {\npub(super) fn foo() {}\nfn bar() {}\npub(crate) fn qux() {}\n}\n";
        let buffer = "mod m {\nfn foo() {}\nfn bar() {}\npub(super) fn qux() {}\n}\n";
        let at = base.find("pub(super)").expect("modifier in base");
        let first = rust_detail(base, buffer).into_iter().next().flatten();
        assert_eq!(
            first.map(|detail| (detail.base_spans.clone(), detail.buffer_spans.clone())),
            Some((
                vec![ChangeSpan {
                    byte_range: at..at + "pub(super)".len(),
                    kind: ChangeKind::Novel,
                    move_metadata: None,
                    prose: ProseChange::None,
                }],
                Vec::new(),
            )),
            "the first hunk marks the removed modifier on the left and nothing on the right",
        );
    }

    /// A call that left one modified line for another keeps its moved span on
    /// both lines, because its counterpart lies outside each line's hunk.
    #[test]
    fn a_token_that_left_the_hunk_keeps_a_moved_span() {
        let base = "fn a() {\n    let v = work(a, b, c) + 1;\n}\nfn b() {\n    let w = 2;\n}\n";
        let buffer = "fn a() {\n    let v = 1;\n}\nfn b() {\n    let w = work(a, b, c) + 2;\n}\n";
        let call = |text: &str| {
            let at = text.find("work(a, b, c)").expect("call");
            at..at + "work(a, b, c)".len()
        };
        assert_eq!(
            rust_hunks(base, buffer)
                .iter()
                .map(moved_extents)
                .collect::<Vec<_>>(),
            [(Some(call(base)), None), (None, Some(call(buffer)))],
            "the call marks moved where it left and where it arrived",
        );
    }

    /// A statement reordered under a new block inside the same hunk is the
    /// reindent noise the tree pass drops, so the hunk carries no moved span.
    #[test]
    fn a_move_inside_its_own_hunk_keeps_no_moved_span() {
        let base = "fn f() {\n    keep();\n    one(a, b);\n    two(c, d);\n}\n";
        let buffer =
            "fn f() {\n    keep();\n    loop {\n        two(c, d);\n        one(a, x);\n    }\n}\n";
        assert_eq!(
            rust_hunks(base, buffer)
                .iter()
                .map(moved_extents)
                .collect::<Vec<_>>(),
            [(None, None)],
            "one hunk, and the statement it relocated marks no move",
        );
    }

    /// The byte extents of `hunk`'s moved spans on its base and buffer sides,
    /// each from the first moved byte to the last.
    fn moved_extents(
        hunk: &DiffHunk,
    ) -> (
        Option<std::ops::Range<usize>>,
        Option<std::ops::Range<usize>>,
    ) {
        let extent = |spans: &[ChangeSpan]| {
            let mut moved = spans.iter().filter(|span| span.kind == ChangeKind::Moved);
            let first = moved.next()?.byte_range.clone();
            Some(moved.fold(first, |extent, span| {
                extent.start.min(span.byte_range.start)..extent.end.max(span.byte_range.end)
            }))
        };
        let detail = hunk.token_detail.as_ref();
        (
            detail.and_then(|detail| extent(&detail.base_spans)),
            detail.and_then(|detail| extent(&detail.buffer_spans)),
        )
    }

    /// One string change reaches the two hunks its first and third lines
    /// make, and each hunk takes the part of it on its own lines.
    #[test]
    fn one_tree_change_details_every_hunk_it_reaches() {
        let span = |at: usize| ChangeSpan {
            byte_range: at..at + 1,
            kind: ChangeKind::Replaced,
            move_metadata: None,
            prose: ProseChange::Part,
        };
        let detail = |at| {
            Some(Arc::new(TokenDetail {
                buffer_spans: vec![span(at)],
                base_spans: vec![span(at)],
                tree: true,
            }))
        };

        assert_eq!(
            rust_detail("let s = \"a\nb\nc\";\n", "let s = \"A\nb\nC\";\n"),
            [detail(9), detail(13)],
            "the string's first and third lines each detail their own hunk",
        );
    }

    /// Each line hunk's token detail once the Rust tree pass of `buffer`
    /// against `base` merges into it.
    fn rust_detail(base: &str, buffer: &str) -> Vec<Option<Arc<TokenDetail>>> {
        rust_hunks(base, buffer)
            .into_iter()
            .map(|hunk| hunk.token_detail)
            .collect()
    }

    /// Each line hunk once the Rust tree pass of `buffer` against `base`
    /// merges into it.
    fn rust_hunks(base: &str, buffer: &str) -> Vec<DiffHunk> {
        let language = stoat_language::LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .expect("rust language");
        let tree = stoat_language::structural_diff::diff_with_language(&language, base, buffer)
            .expect("both sides parse");
        let mut hunks = super::changes_to_hunks(
            &stoat_language::structural_diff::diff(base, buffer).changes,
            base,
            buffer,
        );
        super::merge_structural_detail(&mut hunks, &tree.changes, &tree.unsearched, base, buffer);
        hunks
    }
}
