use crate::{
    display_map::{highlights::HighlightStyle, BlockPlacement, BlockProperties, BlockStyle},
    host::DiffStatus,
};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ops::Range,
    sync::{
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        Arc,
    },
};
use stoat_text::{
    Anchor, Bias, ContextLessSummary, Dimension, Item, KeyedItem, SeekTarget, SumTree,
};

static DIFF_MAP_VERSION_COUNTER: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
pub struct TokenDetail {
    pub buffer_spans: Vec<ChangeSpan>,
    pub base_spans: Vec<ChangeSpan>,
}

#[derive(Clone, Debug)]
pub struct DiffHunk {
    pub status: DiffHunkStatus,
    pub buffer_start_line: u32,
    pub buffer_line_range: Range<u32>,
    pub base_byte_range: Range<usize>,
    pub anchor_range: Option<Range<Anchor>>,
    pub token_detail: Option<Arc<TokenDetail>>,
    /// Whether this hunk's change is already applied to the git index.
    ///
    /// Set only by [`DiffMap::from_structural_changes_staged`]. The plain
    /// [`DiffMap::from_structural_changes`] leaves it `false`, so a map built
    /// without index awareness reads as entirely unstaged.
    pub staged: bool,
}

// --- SumTree plumbing (follows TreeMap/MapKey pattern from text/src/tree_map.rs) ---

#[derive(Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HunkKey(Option<u32>);

impl ContextLessSummary for HunkKey {
    fn add_summary(&mut self, other: &Self) {
        *self = other.clone();
    }
}

#[derive(Clone, Default, Debug)]
struct HunkKeyRef<'a>(Option<&'a u32>);

impl<'a> Dimension<'a, HunkKey> for HunkKeyRef<'a> {
    fn zero(_cx: ()) -> Self {
        Self(None)
    }
    fn add_summary(&mut self, summary: &'a HunkKey, _cx: ()) {
        self.0 = summary.0.as_ref();
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
        HunkKey(Some(self.buffer_start_line))
    }
}

impl KeyedItem for DiffHunk {
    type Key = HunkKey;
    fn key(&self) -> HunkKey {
        HunkKey(Some(self.buffer_start_line))
    }
}

// --- DiffMap ---

/// Syntax highlight spans for the base text, indexed by 0-based base line.
///
/// Each entry holds a line's spans as line-local byte ranges paired with the
/// resolved highlight style, so the diff view's left column can paint base
/// text with tree-sitter token colors.
pub type BaseHighlights = Vec<Vec<(Range<usize>, HighlightStyle)>>;

#[derive(Clone, Debug, Default)]
pub struct DiffMap {
    hunks: SumTree<DiffHunk>,
    base_text: Option<Arc<String>>,
    base_highlights: Option<Arc<BaseHighlights>>,
    /// Base-side underline spans, resolved once at construction from `hunks` and
    /// `base_text` since both are immutable after it. Shared behind `Arc` so the
    /// per-frame accessor hands out a handle instead of rebuilding the map.
    base_underlines: Arc<BTreeMap<u32, Vec<Range<usize>>>>,
    version: usize,
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
        let base_underlines = Arc::new(compute_base_underline_spans(&hunks, base_text.as_ref()));
        Self {
            hunks,
            base_text,
            base_highlights: None,
            base_underlines,
            version: Self::next_version(),
        }
    }

    /// Attach base-text syntax highlights for the diff view's left column.
    pub fn set_base_highlights(&mut self, highlights: Arc<BaseHighlights>) {
        self.base_highlights = Some(highlights);
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
    /// `lhs_text` is the base content the diff was computed against;
    /// `rhs_text` is the buffer content. Adjacent Lhs+Rhs runs from the
    /// diff become [`DiffHunkStatus::Modified`] hunks; isolated runs
    /// become [`DiffHunkStatus::Added`] (Rhs only) or
    /// [`DiffHunkStatus::Deleted`] (Lhs only). The conversion preserves
    /// the original byte ranges so the structural-diff sub-line spans
    /// remain available via [`DiffHunk::token_detail`] in a follow-up.
    pub fn from_structural_changes(
        result: stoat_language::structural_diff::DiffResult,
        lhs_text: &str,
        rhs_text: &str,
    ) -> Self {
        let hunks = changes_to_hunks(&result.changes, lhs_text, rhs_text);
        Self::from_hunks(hunks, Some(Arc::new(lhs_text.to_string())))
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
        lhs_text: &str,
        rhs_text: &str,
        index_changed: &[Range<u32>],
    ) -> Self {
        let mut hunks = changes_to_hunks(&result.changes, lhs_text, rhs_text);
        for hunk in &mut hunks {
            hunk.staged = !index_changed
                .iter()
                .any(|changed| ranges_overlap(changed, &hunk.buffer_line_range));
        }
        Self::from_hunks(hunks, Some(Arc::new(lhs_text.to_string())))
    }

    pub fn version(&self) -> usize {
        self.version
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
    /// the hunk's git-index staged state.
    pub fn gutter_mark_for_line(&self, line: u32) -> Option<(DiffHunkStatus, bool)> {
        let target = HunkKeyRef(Some(&line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        let hunk = cursor.item()?;
        if hunk.buffer_line_range.contains(&line) {
            return Some((hunk.status, hunk.staged));
        }
        if hunk.status == DiffHunkStatus::Deleted && hunk.buffer_start_line == line {
            return Some((DiffHunkStatus::Deleted, hunk.staged));
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
            .map(|hunk| hunk.staged)
    }

    /// Count hunks by staged state as `(staged, unstaged)` for a statusline.
    pub fn staged_counts(&self) -> (usize, usize) {
        self.hunks.iter().fold((0, 0), |(staged, unstaged), hunk| {
            if hunk.staged {
                (staged + 1, unstaged)
            } else {
                (staged, unstaged + 1)
            }
        })
    }

    pub fn has_deletion_after(&self, line: u32) -> bool {
        let target_line = line + 1;
        let target = HunkKeyRef(Some(&target_line));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Left);
        match cursor.item() {
            Some(hunk) => {
                hunk.buffer_start_line == target_line
                    && matches!(
                        hunk.status,
                        DiffHunkStatus::Deleted | DiffHunkStatus::Modified
                    )
                    && !hunk.base_byte_range.is_empty()
            },
            None => false,
        }
    }

    pub fn deleted_blocks(&self) -> Vec<BlockProperties> {
        let base_text = match &self.base_text {
            Some(t) => t,
            None => return Vec::new(),
        };

        self.hunks
            .iter()
            .filter(|h| {
                matches!(h.status, DiffHunkStatus::Deleted | DiffHunkStatus::Modified)
                    && !h.base_byte_range.is_empty()
            })
            .map(|hunk| {
                let content = &base_text[hunk.base_byte_range.clone()];
                let lines: Vec<String> = content.lines().map(String::from).collect();
                let placement_line = hunk.buffer_start_line.saturating_sub(1);
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

    pub fn hunks_in_range(&self, line_range: Range<u32>) -> Vec<&DiffHunk> {
        let mut result = Vec::new();
        let target = HunkKeyRef(Some(&line_range.start));
        let mut cursor = self.hunks.cursor::<HunkKeyRef<'_>>(());
        cursor.seek(&target, Bias::Right);
        cursor.prev();
        // Check if the hunk before the target overlaps
        if let Some(hunk) = cursor.item()
            && hunk.buffer_line_range.end > line_range.start
        {
            result.push(hunk);
        }
        cursor.next();
        while let Some(hunk) = cursor.item() {
            if hunk.buffer_start_line >= line_range.end {
                break;
            }
            result.push(hunk);
            cursor.next();
        }
        result
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

    /// Base-side change spans to underline in the diff view's left column, keyed
    /// by base line with each range line-local within that line.
    ///
    /// Distributes every hunk's [`TokenDetail::base_spans`] of the
    /// [`ChangeKind::Replaced`] kind -- absolute byte ranges in the base text --
    /// across the base lines they cover, so a deleted or modified base block row
    /// underlines only its changed chars. Empty when the map carries no base text.
    pub(crate) fn base_underline_spans(&self) -> Arc<BTreeMap<u32, Vec<Range<usize>>>> {
        self.base_underlines.clone()
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
        self.base_underlines = Arc::new(compute_base_underline_spans(
            &self.hunks,
            self.base_text.as_ref(),
        ));
        self.version = Self::next_version();
    }

    #[cfg(test)]
    pub fn push_hunk(&mut self, hunk: DiffHunk) {
        self.hunks.push(hunk, ());
        self.base_underlines = Arc::new(compute_base_underline_spans(
            &self.hunks,
            self.base_text.as_ref(),
        ));
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

fn changes_to_hunks(
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
                staged: false,
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
                staged: false,
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
            staged: false,
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
                    staged: false,
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
                    staged: false,
                    buffer_start_line: buffer_line,
                    buffer_line_range: buffer_line..buffer_line,
                    base_byte_range: cur.byte_range.clone(),
                    anchor_range: None,
                    token_detail: None,
                });
            },
        }
    }
    hunks.sort_by_key(|h| h.buffer_start_line);
    hunks
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

/// Distribute every hunk's [`ChangeKind::Replaced`] base spans across the base
/// lines they cover, keyed by base line with each range line-local within it.
///
/// Resolved once by [`DiffMap::from_hunks`] because `hunks` and `base_text` are
/// immutable after construction. Empty when there is no base text.
fn compute_base_underline_spans(
    hunks: &SumTree<DiffHunk>,
    base_text: Option<&Arc<String>>,
) -> BTreeMap<u32, Vec<Range<usize>>> {
    let Some(base_text) = base_text else {
        return BTreeMap::new();
    };
    let starts = line_starts(base_text);
    let mut out: BTreeMap<u32, Vec<Range<usize>>> = BTreeMap::new();
    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.base_spans {
            if span.kind == ChangeKind::Replaced {
                distribute_change_span(&mut out, &span.byte_range, &starts, base_text.len());
            }
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
    out: &mut BTreeMap<u32, Vec<Range<usize>>>,
    range: &Range<usize>,
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
                .push((start - line_start)..(end - line_start));
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
    use crate::host::DiffStatus;
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
            staged: false,
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn deleted_hunk(after_line: u32, base_byte_range: std::ops::Range<usize>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Deleted,
            staged: false,
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
            staged: false,
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
        a.staged = true;
        let m = modified_hunk(5..6, 10..14);
        let mut d = deleted_hunk(8, 20..30);
        d.staged = true;

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
            lhs_text,
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
            lhs_text,
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
    fn base_underline_spans_split_across_base_lines() {
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
            lhs_text,
            rhs_text,
        );

        let flat: Vec<(u32, usize, usize)> = dm
            .base_underline_spans()
            .iter()
            .flat_map(|(&line, ranges)| ranges.iter().map(move |r| (line, r.start, r.end)))
            .collect();
        assert_eq!(
            flat,
            vec![(0, 0, 5), (1, 0, 4)],
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
            lhs_text,
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
            lhs_text,
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
        assert!(!dm.has_deletion_after(0));
        assert!(dm.is_empty());
        assert_eq!(dm.total_deleted_lines(), 0);
        assert!(dm.deleted_blocks().is_empty());
    }

    #[test]
    fn single_added_hunk() {
        let dm = DiffMap::from_hunks([added_hunk(5..8)], None);

        assert_eq!(dm.status_for_line(4), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(5), DiffStatus::Added);
        assert_eq!(dm.status_for_line(6), DiffStatus::Added);
        assert_eq!(dm.status_for_line(7), DiffStatus::Added);
        assert_eq!(dm.status_for_line(8), DiffStatus::Unchanged);
        assert!(!dm.has_deletion_after(4));
        assert!(dm.deleted_blocks().is_empty());
    }

    #[test]
    fn single_deleted_hunk() {
        let base = "deleted line\n";
        let dm = DiffMap::from_hunks([deleted_hunk(2, 0..13)], Some(Arc::new(base.to_string())));

        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
        assert!(dm.has_deletion_after(2));
        assert!(!dm.has_deletion_after(1));
        assert!(!dm.has_deletion_after(3));

        let blocks = dm.deleted_blocks();
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
        assert!(dm.has_deletion_after(2));

        let blocks = dm.deleted_blocks();
        assert_eq!(blocks.len(), 1);
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
        assert!(dm.has_deletion_after(4));
        assert_eq!(dm.status_for_line(7), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(8), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(9), DiffStatus::Unchanged);

        assert_eq!(dm.deleted_blocks().len(), 2);
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
        assert!(dm.has_deletion_after(0));
        assert!(!dm.has_deletion_after(1));
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
        let dm = DiffMap::from_structural_changes(result, lhs, rhs);
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
        let dm = DiffMap::from_structural_changes(result, lhs, rhs);
        assert_eq!(dm.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(dm.status_for_line(1), DiffStatus::Modified);
        assert_eq!(dm.status_for_line(2), DiffStatus::Unchanged);
    }

    #[test]
    fn from_structural_changes_identical_inputs() {
        let txt = "one\ntwo\nthree\n";
        let result = stoat_language::structural_diff::diff(txt, txt);
        let dm = DiffMap::from_structural_changes(result, txt, txt);
        assert!(dm.is_empty());
    }

    #[test]
    fn from_structural_changes_leaves_hunks_unstaged() {
        let lhs = "a\nb\nc\n";
        let rhs = "a\nB\nc\n";
        let result = stoat_language::structural_diff::diff(lhs, rhs);
        let dm = DiffMap::from_structural_changes(result, lhs, rhs);
        assert!(
            dm.hunks_in_range(0..u32::MAX).iter().all(|h| !h.staged),
            "index-unaware construction reads as entirely unstaged"
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
            index,
            buffer,
        )
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| h.buffer_line_range.clone())
        .collect();
        let result = stoat_language::structural_diff::diff(base, buffer);
        let dm = DiffMap::from_structural_changes_staged(result, base, buffer, &index_changed);
        let flags: Vec<(u32, bool)> = dm
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|h| (h.buffer_start_line, h.staged))
            .collect();
        assert_eq!(
            flags,
            vec![(1, true), (3, false)],
            "line-1 change staged, line-3 change unstaged"
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
        let dm = DiffMap::from_structural_changes(result, lhs_text, rhs_text);

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
            lhs_text,
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
