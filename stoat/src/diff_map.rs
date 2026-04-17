use crate::{
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    host::DiffStatus,
};
use std::{
    cmp::Ordering,
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

#[derive(Clone, Debug, Default)]
pub struct DiffMap {
    hunks: SumTree<DiffHunk>,
    base_text: Option<Arc<String>>,
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
        Self {
            hunks: SumTree::from_iter(hunks, ()),
            base_text,
            version: Self::next_version(),
        }
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
        if let Some(hunk) = cursor.item() {
            if hunk.buffer_line_range.end > line_range.start {
                result.push(hunk);
            }
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
        self.version = Self::next_version();
    }

    #[cfg(test)]
    pub fn push_hunk(&mut self, hunk: DiffHunk) {
        self.hunks.push(hunk, ());
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
fn changes_to_hunks(
    changes: &[stoat_language::structural_diff::DiffChange],
    lhs_text: &str,
    rhs_text: &str,
) -> Vec<DiffHunk> {
    use std::collections::HashMap;
    use stoat_language::structural_diff::{ChangeKind as LangChangeKind, Side};

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
            let first = &changes[*rhs_indices.first().unwrap()];
            let last = &changes[*rhs_indices.last().unwrap()];
            let full_range = first.byte_range.start..last.byte_range.end;
            let line_range = byte_range_to_line_range(rhs_text, &full_range);
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
            let first = &changes[*lhs_indices.first().unwrap()];
            let last = &changes[*lhs_indices.last().unwrap()];
            let full_range = first.byte_range.start..last.byte_range.end;
            let lhs_line = lhs_text[..first.byte_range.start.min(lhs_text.len())]
                .chars()
                .filter(|c| *c == '\n')
                .count() as u32;
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
        let line_range = byte_range_to_line_range(rhs_text, &rhs_change.byte_range);
        hunks.push(DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: lhs_change.byte_range.clone(),
            anchor_range: None,
            token_detail: None,
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
                let line_range = byte_range_to_line_range(rhs_text, &cur.byte_range);
                hunks.push(DiffHunk {
                    status: DiffHunkStatus::Added,
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
                let buffer_line = cur.deletion_rhs_anchor.unwrap_or_else(|| {
                    lhs_text[..cur.byte_range.start.min(lhs_text.len())]
                        .chars()
                        .filter(|c| *c == '\n')
                        .count() as u32
                });
                hunks.push(DiffHunk {
                    status: DiffHunkStatus::Deleted,
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

fn byte_range_to_line_range(text: &str, byte_range: &Range<usize>) -> Range<u32> {
    let start_byte = byte_range.start.min(text.len());
    let end_byte = byte_range.end.min(text.len());
    let start_line = text[..start_byte].chars().filter(|c| *c == '\n').count() as u32;
    let end_line_inclusive = text[..end_byte].chars().filter(|c| *c == '\n').count() as u32;
    // For an empty range, return start..start so callers can detect it.
    if start_byte == end_byte {
        return start_line..start_line;
    }
    start_line..(end_line_inclusive + 1)
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail};
    use crate::host::DiffStatus;
    use std::sync::Arc;

    fn added_hunk(line_range: std::ops::Range<u32>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Added,
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
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range,
            anchor_range: None,
            token_detail: None,
        }
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
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..5,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(0),
                deletion_rhs_anchor: None,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 11..16,
                kind: LangChangeKind::Replaced,
                move_metadata: None,
                pair_id: Some(1),
                deletion_rhs_anchor: None,
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
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(rhs_meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
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
            },
            DiffChange {
                side: Side::Lhs,
                byte_range: 0..18,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
            },
            DiffChange {
                side: Side::Rhs,
                byte_range: 32..51,
                kind: LangChangeKind::Moved,
                move_metadata: Some(meta.clone()),
                pair_id: None,
                deletion_rhs_anchor: None,
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
