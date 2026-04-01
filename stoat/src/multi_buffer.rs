use crate::{
    buffer::{BufferId, SharedBuffer, TextBufferSnapshot},
    diff_map::DiffMap,
};
use std::{ops::Range, sync::OnceLock};
use stoat_text::{
    patch::Patch, Anchor, Bias, ContextLessSummary, Dimension, Item, KeyedItem, Locator, Point,
    Rope, SumTree, TextSummary,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExcerptId(u64);

impl ExcerptId {
    pub fn min() -> Self {
        Self(u64::MIN)
    }

    pub fn max() -> Self {
        Self(u64::MAX)
    }
}

impl Default for ExcerptId {
    fn default() -> Self {
        Self::min()
    }
}

impl ContextLessSummary for ExcerptId {
    fn add_summary(&mut self, summary: &Self) {
        *self = *summary;
    }
}

/// A stable reference to a position within a [`MultiBuffer`].
///
/// Wraps a [`stoat_text::Anchor`] (buffer-local position) with an [`ExcerptId`]
/// to identify which excerpt the position belongs to, and an optional
/// [`diff_base_anchor`](MultiBufferAnchor::diff_base_anchor) for referencing
/// positions in the diff base (original) text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MultiBufferAnchor {
    pub excerpt_id: ExcerptId,
    pub text_anchor: Anchor,
    pub diff_base_anchor: Option<Anchor>,
}

impl MultiBufferAnchor {
    pub fn min() -> Self {
        Self {
            excerpt_id: ExcerptId::min(),
            text_anchor: Anchor::min(),
            diff_base_anchor: None,
        }
    }

    pub fn max() -> Self {
        Self {
            excerpt_id: ExcerptId::max(),
            text_anchor: Anchor::max(),
            diff_base_anchor: None,
        }
    }

    pub fn in_buffer(excerpt_id: ExcerptId, text_anchor: Anchor) -> Self {
        Self {
            excerpt_id,
            text_anchor,
            diff_base_anchor: None,
        }
    }

    pub fn with_diff_base_anchor(self, diff_base_anchor: Anchor) -> Self {
        Self {
            diff_base_anchor: Some(diff_base_anchor),
            ..self
        }
    }

    pub fn is_min(&self) -> bool {
        self.excerpt_id == ExcerptId::min()
            && self.text_anchor.is_min()
            && self.diff_base_anchor.is_none()
    }

    pub fn is_max(&self) -> bool {
        self.excerpt_id == ExcerptId::max()
            && self.text_anchor.is_max()
            && self.diff_base_anchor.is_none()
    }

    pub fn bias(&self) -> Bias {
        self.text_anchor.bias
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MultiBufferPoint {
    pub row: u32,
    pub column: u32,
}

impl MultiBufferPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MultiBufferRow(pub u32);

#[derive(Clone, Debug)]
pub struct ExcerptInfo {
    pub id: ExcerptId,
    pub buffer_id: BufferId,
}

#[derive(Clone, Debug)]
pub struct ExcerptBoundary {
    pub prev: Option<ExcerptInfo>,
    pub next: ExcerptInfo,
    pub row: u32,
}

impl ExcerptBoundary {
    pub fn starts_new_buffer(&self) -> bool {
        self.prev
            .as_ref()
            .is_none_or(|p| p.buffer_id != self.next.buffer_id)
    }
}

// ---- Excerpt SumTree infrastructure ----

#[derive(Clone)]
struct ExcerptEntry {
    id: ExcerptId,
    locator: Locator,
    buffer_id: BufferId,
    buffer_snapshot: TextBufferSnapshot,
    range: Range<Anchor>,
    text_summary: TextSummary,
    has_trailing_newline: bool,
}

#[derive(Clone, Default, Debug)]
struct ExcerptSummary {
    excerpt_id: ExcerptId,
    excerpt_locator: Locator,
    text: TextSummary,
    count: usize,
}

impl ContextLessSummary for ExcerptSummary {
    fn add_summary(&mut self, other: &Self) {
        self.excerpt_id = other.excerpt_id;
        self.excerpt_locator.assign(&other.excerpt_locator);
        ContextLessSummary::add_summary(&mut self.text, &other.text);
        self.count += other.count;
    }
}

impl Item for ExcerptEntry {
    type Summary = ExcerptSummary;

    fn summary(&self, _cx: ()) -> ExcerptSummary {
        let mut text = self.text_summary.clone();
        if self.has_trailing_newline {
            text.len += 1;
            text.lines.row += 1;
            text.lines.column = 0;
        }
        ExcerptSummary {
            excerpt_id: self.id,
            excerpt_locator: self.locator.clone(),
            text,
            count: 1,
        }
    }
}

// Dimension: cumulative byte offset
impl<'a> Dimension<'a, ExcerptSummary> for usize {
    fn zero(_cx: ()) -> Self {
        0
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _cx: ()) {
        *self += summary.text.len;
    }
}

// Dimension: cumulative lines (Point)
impl<'a> Dimension<'a, ExcerptSummary> for Point {
    fn zero(_cx: ()) -> Self {
        Point::new(0, 0)
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _cx: ()) {
        *self = Point::new(
            self.row + summary.text.lines.row,
            if summary.text.lines.row > 0 {
                summary.text.lines.column
            } else {
                self.column + summary.text.lines.column
            },
        );
    }
}

// Dimension: max excerpt ID
impl<'a> Dimension<'a, ExcerptSummary> for Option<ExcerptId> {
    fn zero(_cx: ()) -> Self {
        None
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _cx: ()) {
        *self = Some(summary.excerpt_id);
    }
}

// ExcerptIdMapping for O(log n) ID-to-Locator lookup
#[derive(Clone, Debug)]
struct ExcerptIdMapping {
    id: ExcerptId,
    locator: Locator,
}

impl Item for ExcerptIdMapping {
    type Summary = ExcerptId;

    fn summary(&self, _cx: ()) -> ExcerptId {
        self.id
    }
}

impl KeyedItem for ExcerptIdMapping {
    type Key = ExcerptId;

    fn key(&self) -> Self::Key {
        self.id
    }
}

// ---- Mutable owner (holds live buffers) ----

struct LiveExcerpt {
    id: ExcerptId,
    buffer_id: BufferId,
    buffer: SharedBuffer,
}

pub struct MultiBuffer {
    live_excerpts: Vec<LiveExcerpt>,
    excerpt_tree: SumTree<ExcerptEntry>,
    excerpt_ids: SumTree<ExcerptIdMapping>,
    next_excerpt_id: u64,
    singleton: bool,
}

impl MultiBuffer {
    pub fn singleton(buffer_id: BufferId, buffer: SharedBuffer) -> Self {
        let id = ExcerptId(0);
        let locator = Locator::between(Locator::min_ref(), Locator::max_ref());

        let buffer_snapshot = buffer
            .read()
            .expect("buffer lock poisoned")
            .snapshot
            .clone();
        let text_summary = buffer_snapshot.visible_text.summary().clone();

        let mut excerpt_tree = SumTree::new(());
        excerpt_tree.push(
            ExcerptEntry {
                id,
                locator: locator.clone(),
                buffer_id,
                buffer_snapshot,
                range: Anchor::min()..Anchor::max(),
                text_summary,
                has_trailing_newline: false,
            },
            (),
        );

        let mut excerpt_ids = SumTree::new(());
        excerpt_ids.push(ExcerptIdMapping { id, locator }, ());

        Self {
            live_excerpts: vec![LiveExcerpt {
                id,
                buffer_id,
                buffer,
            }],
            excerpt_tree,
            excerpt_ids,
            next_excerpt_id: 1,
            singleton: true,
        }
    }

    pub fn is_singleton(&self) -> bool {
        self.singleton
    }

    pub fn buffer_version(&self) -> u64 {
        let excerpt = &self.live_excerpts[0];
        let buffer = excerpt.buffer.read().expect("buffer lock poisoned");
        buffer.version()
    }

    pub fn snapshot(&self) -> MultiBufferSnapshot {
        if self.singleton {
            let excerpt = &self.live_excerpts[0];
            let buffer = excerpt.buffer.read().expect("buffer lock poisoned");
            MultiBufferSnapshot {
                singleton: true,
                buffer_snapshot: buffer.snapshot.clone(),
                text_cache: OnceLock::new(),
                diff_map: buffer.diff_map.clone(),
                excerpt_tree: None,
                excerpt_ids: None,
            }
        } else {
            // Multi-excerpt: rebuild excerpt tree with fresh buffer snapshots
            let cx = ();
            let mut tree = SumTree::new(cx);
            let mut ids = SumTree::new(cx);
            for (i, live) in self.live_excerpts.iter().enumerate() {
                let buf = live.buffer.read().expect("buffer lock poisoned");
                let text_summary = buf.snapshot.visible_text.summary().clone();
                let locator = self
                    .excerpt_tree
                    .items(())
                    .get(i)
                    .map(|e| e.locator.clone())
                    .unwrap_or_else(|| Locator::between(Locator::min_ref(), Locator::max_ref()));
                let has_trailing_newline = i < self.live_excerpts.len() - 1;
                tree.push(
                    ExcerptEntry {
                        id: live.id,
                        locator: locator.clone(),
                        buffer_id: live.buffer_id,
                        buffer_snapshot: buf.snapshot.clone(),
                        range: Anchor::min()..Anchor::max(),
                        text_summary,
                        has_trailing_newline,
                    },
                    cx,
                );
                ids.push(
                    ExcerptIdMapping {
                        id: live.id,
                        locator,
                    },
                    cx,
                );
            }
            let first_buf = self.live_excerpts[0].buffer.read().expect("lock");
            MultiBufferSnapshot {
                singleton: false,
                buffer_snapshot: first_buf.snapshot.clone(),
                text_cache: OnceLock::new(),
                diff_map: first_buf.diff_map.clone(),
                excerpt_tree: Some(tree),
                excerpt_ids: Some(ids),
            }
        }
    }

    pub fn insert_excerpts(
        &mut self,
        buffer_id: BufferId,
        buffer: SharedBuffer,
        ranges: Vec<Range<usize>>,
    ) -> Vec<ExcerptId> {
        let buf = buffer.read().expect("buffer lock poisoned");
        let mut new_ids = Vec::with_capacity(ranges.len());

        for range in ranges {
            let id = ExcerptId(self.next_excerpt_id);
            self.next_excerpt_id += 1;

            let prev_locator = self
                .live_excerpts
                .last()
                .and_then(|_| self.excerpt_tree.last().map(|e| &e.locator))
                .unwrap_or(Locator::min_ref());
            let locator = Locator::between(prev_locator, Locator::max_ref());

            let start_anchor = buf.snapshot.anchor_at(range.start, Bias::Left);
            let end_anchor = buf.snapshot.anchor_at(range.end, Bias::Right);
            let text_summary = buf
                .snapshot
                .visible_text
                .text_summary_for_range(range.clone());

            let has_trailing_newline = true;

            self.excerpt_tree.push(
                ExcerptEntry {
                    id,
                    locator: locator.clone(),
                    buffer_id,
                    buffer_snapshot: buf.snapshot.clone(),
                    range: start_anchor..end_anchor,
                    text_summary,
                    has_trailing_newline,
                },
                (),
            );
            self.excerpt_ids.push(
                ExcerptIdMapping {
                    id,
                    locator: locator.clone(),
                },
                (),
            );
            self.live_excerpts.push(LiveExcerpt {
                id,
                buffer_id,
                buffer: buffer.clone(),
            });

            new_ids.push(id);
        }

        // Last excerpt shouldn't have trailing newline
        if let Some(last) = self.excerpt_tree.last() {
            if last.has_trailing_newline {
                self.excerpt_tree.update_last(
                    |entry| {
                        entry.has_trailing_newline = false;
                    },
                    (),
                );
            }
        }

        self.singleton = false;
        new_ids
    }

    pub fn remove_excerpts(&mut self, ids: &[ExcerptId]) {
        use std::collections::HashSet;
        let id_set: HashSet<ExcerptId> = ids.iter().copied().collect();
        self.live_excerpts.retain(|e| !id_set.contains(&e.id));

        let cx = ();
        let mut new_tree = SumTree::new(cx);
        let mut new_ids = SumTree::new(cx);
        for entry in self.excerpt_tree.items(()) {
            if !id_set.contains(&entry.id) {
                let mapping = ExcerptIdMapping {
                    id: entry.id,
                    locator: entry.locator.clone(),
                };
                new_tree.push(entry, cx);
                new_ids.push(mapping, cx);
            }
        }

        if let Some(last) = new_tree.last() {
            if last.has_trailing_newline {
                new_tree.update_last(|entry| entry.has_trailing_newline = false, cx);
            }
        }

        self.excerpt_tree = new_tree;
        self.excerpt_ids = new_ids;

        if self.live_excerpts.len() <= 1 {
            self.singleton = self.live_excerpts.len() == 1;
        }
    }

    pub fn as_singleton(&self) -> Option<&SharedBuffer> {
        if self.singleton {
            Some(&self.live_excerpts[0].buffer)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct MultiBufferSnapshot {
    singleton: bool,
    buffer_snapshot: TextBufferSnapshot,
    text_cache: OnceLock<String>,
    pub diff_map: Option<DiffMap>,
    excerpt_tree: Option<SumTree<ExcerptEntry>>,
    excerpt_ids: Option<SumTree<ExcerptIdMapping>>,
}

impl MultiBufferSnapshot {
    pub fn show_headers(&self) -> bool {
        !self.singleton
    }

    pub fn excerpt_boundaries_in_range(
        &self,
        range: Range<u32>,
    ) -> impl Iterator<Item = ExcerptBoundary> + '_ {
        let tree = match &self.excerpt_tree {
            Some(t) if !self.singleton => t,
            _ => return ExcerptBoundaryIter::empty(),
        };

        let mut cursor = tree.cursor::<Point>(());
        cursor.seek(&Point::new(range.start, 0), Bias::Right);

        let mut prev_info: Option<ExcerptInfo> = None;
        let mut boundaries = Vec::new();

        while let Some(entry) = cursor.item() {
            let row = cursor.start().row;
            if row > range.end {
                break;
            }
            let info = ExcerptInfo {
                id: entry.id,
                buffer_id: entry.buffer_id,
            };
            boundaries.push(ExcerptBoundary {
                prev: prev_info.clone(),
                next: info.clone(),
                row,
            });
            prev_info = Some(info);
            cursor.next();
        }

        ExcerptBoundaryIter(boundaries.into_iter())
    }

    pub fn empty() -> Self {
        Self {
            singleton: true,
            buffer_snapshot: TextBufferSnapshot::empty(),
            text_cache: OnceLock::new(),
            diff_map: None,
            excerpt_tree: None,
            excerpt_ids: None,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.buffer_snapshot.visible_text
    }

    pub fn line_count(&self) -> u32 {
        self.buffer_snapshot.line_count()
    }

    pub fn text(&self) -> &str {
        self.text_cache
            .get_or_init(|| self.buffer_snapshot.visible_text.to_string())
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text().split('\n')
    }

    pub fn line_at_row(&self, row: u32) -> String {
        self.buffer_snapshot.visible_text.line_at_row(row)
    }

    pub fn max_point(&self) -> MultiBufferPoint {
        let p = self.buffer_snapshot.max_point();
        MultiBufferPoint::new(p.row, p.column)
    }

    pub fn point_to_multi_buffer_point(&self, point: Point) -> MultiBufferPoint {
        MultiBufferPoint {
            row: point.row,
            column: point.column,
        }
    }

    pub fn multi_buffer_point_to_point(&self, point: MultiBufferPoint) -> Point {
        Point::new(point.row, point.column)
    }

    pub fn anchor_at(&self, offset: usize, bias: Bias) -> Anchor {
        self.buffer_snapshot.anchor_at(offset, bias)
    }

    pub fn resolve_anchor(&self, anchor: &Anchor) -> usize {
        self.buffer_snapshot.resolve_anchor(anchor)
    }

    pub fn point_for_anchor(&self, anchor: &Anchor) -> Point {
        self.buffer_snapshot.point_for_anchor(anchor)
    }

    pub fn resolve_anchors_batch(&self, anchors: &[Anchor]) -> Vec<usize> {
        self.buffer_snapshot.resolve_anchors_batch(anchors)
    }

    pub fn points_for_anchors_batch(&self, anchors: &[Anchor]) -> Vec<Point> {
        self.buffer_snapshot.points_for_anchors_batch(anchors)
    }

    pub fn is_anchor_valid(&self, anchor: &Anchor) -> bool {
        self.buffer_snapshot.is_anchor_valid(anchor)
    }

    pub fn cmp_anchors(&self, a: &Anchor, b: &Anchor) -> std::cmp::Ordering {
        a.cmp(b, &|anchor| self.resolve_anchor(anchor))
    }

    pub fn edits_since(&self, since_version: u64) -> Patch<usize> {
        self.buffer_snapshot.edits_since(since_version)
    }

    pub fn version(&self) -> u64 {
        self.buffer_snapshot.version
    }

    pub fn is_singleton(&self) -> bool {
        self.singleton
    }

    pub fn multi_buffer_anchor_at(&self, offset: usize, bias: Bias) -> MultiBufferAnchor {
        let text_anchor = self.buffer_snapshot.anchor_at(offset, bias);
        MultiBufferAnchor::in_buffer(ExcerptId(0), text_anchor)
    }

    pub fn resolve_multi_buffer_anchor(&self, anchor: &MultiBufferAnchor) -> usize {
        self.buffer_snapshot.resolve_anchor(&anchor.text_anchor)
    }

    pub fn cmp_multi_buffer_anchors(
        &self,
        a: &MultiBufferAnchor,
        b: &MultiBufferAnchor,
    ) -> std::cmp::Ordering {
        let text_cmp = a.text_anchor.cmp(&b.text_anchor, &|anchor| {
            self.buffer_snapshot.resolve_anchor(anchor)
        });
        if text_cmp.is_ne() {
            return text_cmp;
        }
        match (&a.diff_base_anchor, &b.diff_base_anchor) {
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
            (Some(a_base), Some(b_base)) => a_base.cmp(b_base, &|anchor| {
                self.buffer_snapshot.resolve_anchor(anchor)
            }),
        }
    }

    fn excerpt_locator_for_id(&self, id: ExcerptId) -> Option<&Locator> {
        if id == ExcerptId::min() {
            return Some(Locator::min_ref());
        }
        if id == ExcerptId::max() {
            return Some(Locator::max_ref());
        }
        let ids = self.excerpt_ids.as_ref()?;
        let (_, _, item) = ids.find::<ExcerptId, _>((), &id, Bias::Left);
        item.filter(|entry| entry.id == id)
            .map(|entry| &entry.locator)
    }
}

struct ExcerptBoundaryIter(std::vec::IntoIter<ExcerptBoundary>);

impl ExcerptBoundaryIter {
    fn empty() -> Self {
        Self(Vec::new().into_iter())
    }
}

impl Iterator for ExcerptBoundaryIter {
    type Item = ExcerptBoundary;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

#[cfg(test)]
mod tests {
    use super::{MultiBuffer, MultiBufferAnchor, MultiBufferPoint};
    use crate::buffer::{BufferId, TextBuffer};
    use std::sync::{Arc, RwLock};
    use stoat_text::Point;

    fn create_test_buffer(content: &str) -> (BufferId, Arc<RwLock<TextBuffer>>) {
        let id = BufferId::new(0);
        let buffer = TextBuffer::with_text(id, content);
        (id, Arc::new(RwLock::new(buffer)))
    }

    #[test]
    fn singleton_creation() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        assert!(multi.is_singleton());
        assert!(multi.as_singleton().is_some());
    }

    #[test]
    fn snapshot_line_count() {
        let (id, buffer) = create_test_buffer("line1\nline2\nline3");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn snapshot_text() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.text(), "hello\nworld");
    }

    #[test]
    fn snapshot_lines() {
        let (id, buffer) = create_test_buffer("a\nb\nc");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let lines: Vec<_> = snapshot.lines().collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn snapshot_max_point() {
        let (id, buffer) = create_test_buffer("short\nlonger line\nx");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let max = snapshot.max_point();
        assert_eq!(max.row, 2);
        assert_eq!(max.column, 1);
    }

    #[test]
    fn anchor_valid_within_bounds() {
        let (id, buffer) = create_test_buffer("0123456789");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_max_is_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Left);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn passthrough_coordinates() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();

        let point = Point::new(1, 3);
        let mb_point = snapshot.point_to_multi_buffer_point(point);
        assert_eq!(mb_point, MultiBufferPoint::new(1, 3));

        let back = snapshot.multi_buffer_point_to_point(mb_point);
        assert_eq!(back, point);
    }

    #[test]
    fn stale_anchor_invalid_after_edit() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snap1 = multi.snapshot();
        let anchor = snap1.anchor_at(2, stoat_text::Bias::Right);

        multi
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..0, "XX");

        let snap2 = multi.snapshot();
        assert!(snap2.is_anchor_valid(&anchor));
    }

    #[test]
    fn fresh_anchor_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(3, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_min_max_always_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);

        multi
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..0, "XX");

        let snapshot = multi.snapshot();
        assert!(snapshot.is_anchor_valid(&stoat_text::Anchor::min()));
        assert!(snapshot.is_anchor_valid(&stoat_text::Anchor::max()));
    }

    #[test]
    fn edits_since_single_edit() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer.clone());
        let v0 = multi.snapshot().version();
        buffer.write().unwrap().edit(0..0, "XX");
        let snap = multi.snapshot();
        let patch = snap.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, 0..0);
        assert_eq!(edits[0].new, 0..2);
    }

    #[test]
    fn edits_since_multiple_edits() {
        let (id, buffer) = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(id, buffer.clone());
        let v0 = multi.snapshot().version();
        buffer.write().unwrap().edit(0..0, "XX");
        buffer.write().unwrap().edit(8..11, "Y");
        let snap = multi.snapshot();
        let patch = snap.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].old, 0..0);
        assert_eq!(edits[0].new, 0..2);
        assert_eq!(edits[1].old, 6..9);
        assert_eq!(edits[1].new, 8..9);
    }

    #[test]
    fn edits_since_no_changes() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snap = multi.snapshot();
        let patch = snap.edits_since(snap.version());
        assert!(patch.is_empty());
    }

    #[test]
    fn multi_buffer_anchor_creation() {
        let (id, buffer) = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let mb_anchor = snapshot.multi_buffer_anchor_at(5, stoat_text::Bias::Right);
        assert_eq!(mb_anchor.excerpt_id, super::ExcerptId(0));
        assert!(!mb_anchor.is_min());
        assert!(!mb_anchor.is_max());
        assert_eq!(mb_anchor.diff_base_anchor, None);

        let offset = snapshot.resolve_multi_buffer_anchor(&mb_anchor);
        assert_eq!(offset, 5);
    }

    #[test]
    fn multi_buffer_anchor_min_max() {
        let min = MultiBufferAnchor::min();
        let max = MultiBufferAnchor::max();
        assert!(min.is_min());
        assert!(max.is_max());
        assert!(!min.is_max());
        assert!(!max.is_min());
    }

    #[test]
    fn multi_buffer_anchor_with_diff_base() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let text_anchor = snapshot.anchor_at(3, stoat_text::Bias::Right);
        let base_anchor = snapshot.anchor_at(1, stoat_text::Bias::Left);
        let mb_anchor = MultiBufferAnchor::in_buffer(super::ExcerptId(0), text_anchor)
            .with_diff_base_anchor(base_anchor);
        assert_eq!(mb_anchor.diff_base_anchor, Some(base_anchor));
    }

    #[test]
    fn cmp_multi_buffer_anchors() {
        let (id, buffer) = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let a = snapshot.multi_buffer_anchor_at(3, stoat_text::Bias::Left);
        let b = snapshot.multi_buffer_anchor_at(7, stoat_text::Bias::Left);
        assert_eq!(
            snapshot.cmp_multi_buffer_anchors(&a, &b),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            snapshot.cmp_multi_buffer_anchors(&b, &a),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            snapshot.cmp_multi_buffer_anchors(&a, &a),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn cmp_anchors_by_offset() {
        let (id, buffer) = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let a = snapshot.anchor_at(3, stoat_text::Bias::Left);
        let b = snapshot.anchor_at(7, stoat_text::Bias::Left);
        assert_eq!(snapshot.cmp_anchors(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(snapshot.cmp_anchors(&b, &a), std::cmp::Ordering::Greater);
        assert_eq!(snapshot.cmp_anchors(&a, &a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn insert_excerpts_multi_buffer() {
        let id1 = BufferId::new(1);
        let buf1 = TextBuffer::with_text(id1, "hello");
        let shared1 = Arc::new(RwLock::new(buf1));

        let id2 = BufferId::new(2);
        let buf2 = TextBuffer::with_text(id2, "world");
        let shared2 = Arc::new(RwLock::new(buf2));

        let mut multi = MultiBuffer::singleton(id1, shared1);
        let new_ids = multi.insert_excerpts(id2, shared2, vec![0..5]);
        assert_eq!(new_ids.len(), 1);
        assert!(!multi.is_singleton());
        assert_eq!(multi.live_excerpts.len(), 2);
    }

    #[test]
    fn excerpt_boundaries_singleton_empty() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let boundaries: Vec<_> = snapshot.excerpt_boundaries_in_range(0..10).collect();
        assert!(boundaries.is_empty());
    }

    #[test]
    fn remove_excerpts_from_multi() {
        let id1 = BufferId::new(1);
        let buf1 = TextBuffer::with_text(id1, "aaa");
        let shared1 = Arc::new(RwLock::new(buf1));

        let id2 = BufferId::new(2);
        let buf2 = TextBuffer::with_text(id2, "bbb");
        let shared2 = Arc::new(RwLock::new(buf2));

        let mut multi = MultiBuffer::singleton(id1, shared1);
        let new_ids = multi.insert_excerpts(id2, shared2, vec![0..3]);
        assert_eq!(multi.live_excerpts.len(), 2);

        multi.remove_excerpts(&new_ids);
        assert_eq!(multi.live_excerpts.len(), 1);
        assert!(multi.is_singleton());
    }
}
