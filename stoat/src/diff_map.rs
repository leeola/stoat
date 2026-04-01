use crate::{
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    git::DiffStatus,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Novel,
    Replaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSpan {
    pub byte_range: Range<usize>,
    pub kind: ChangeKind,
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

#[cfg(test)]
mod tests {
    use super::{ChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail};
    use crate::git::DiffStatus;
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
}
