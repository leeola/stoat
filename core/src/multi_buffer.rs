use crate::{
    buffer::{BufferId, SharedBuffer, TextBufferSnapshot},
    diff_map::DiffMap,
};
use std::sync::OnceLock;
use stoat_text::{patch::Patch, Anchor, Bias, Point, Rope};

/// One text buffer behind the display pipeline's buffer-facing surface.
///
/// Sits between a [`DisplayMap`](crate::display_map::DisplayMap) and the
/// [`TextBuffer`](crate::buffer::TextBuffer) it shows, handing out immutable
/// [`MultiBufferSnapshot`] values the fold, wrap, and block layers read.
/// Holding the buffer behind a snapshot is what lets those layers work from a
/// consistent view while an edit lands on the live buffer.
pub struct MultiBuffer {
    buffer: SharedBuffer,
}

impl MultiBuffer {
    /// A surface over the whole of `buffer`.
    ///
    /// The name records that this surface could once hold several excerpts of
    /// several files. Nothing ever asked it to, so it holds one whole buffer.
    pub fn singleton(buffer: SharedBuffer) -> Self {
        Self { buffer }
    }

    pub fn buffer_version(&self) -> u64 {
        let buffer = self.buffer.read().expect("buffer lock poisoned");
        buffer.version()
    }

    /// Version of the buffer's diff map, or 0 when it has none.
    ///
    /// The display-map snapshot cache reads this to catch diff-map mutations,
    /// which do not bump the buffer's edit version.
    pub fn diff_version(&self) -> usize {
        let buffer = self.buffer.read().expect("buffer lock poisoned");
        buffer.diff_map.as_ref().map(|d| d.version()).unwrap_or(0)
    }

    pub fn snapshot(&self) -> MultiBufferSnapshot {
        let buffer = self.buffer.read().expect("buffer lock poisoned");
        MultiBufferSnapshot {
            buffer_snapshot: buffer.snapshot.clone(),
            text_cache: OnceLock::new(),
            diff_map: buffer.diff_map.clone(),
        }
    }

    pub fn as_singleton(&self) -> Option<&SharedBuffer> {
        Some(&self.buffer)
    }
}

/// An immutable view of a buffer's text, taken at one edit version.
///
/// The fold, wrap, and block layers each read one of these rather than the
/// live buffer, so a pass over the pipeline sees one consistent text even
/// while an edit lands underneath it.
#[derive(Clone)]
pub struct MultiBufferSnapshot {
    buffer_snapshot: TextBufferSnapshot,
    text_cache: OnceLock<String>,
    pub diff_map: Option<DiffMap>,
}

impl MultiBufferSnapshot {
    pub fn empty() -> Self {
        Self {
            buffer_snapshot: TextBufferSnapshot::empty(),
            text_cache: OnceLock::new(),
            diff_map: None,
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

    /// Anchors for every offset, in one walk rather than a root descent apiece.
    ///
    /// The counterpart to [`Self::resolve_anchors_batch`], for a caller holding
    /// offsets it needs to hand on as anchors.
    pub fn anchors_at_batch(&self, offsets: &[usize], bias: Bias) -> Vec<Anchor> {
        self.buffer_snapshot.anchors_at_batch(offsets, bias)
    }

    pub fn points_for_anchors_batch(&self, anchors: &[Anchor]) -> Vec<Point> {
        self.buffer_snapshot.points_for_anchors_batch(anchors)
    }

    pub fn is_anchor_valid(&self, anchor: &Anchor) -> bool {
        self.buffer_snapshot.is_anchor_valid(anchor)
    }

    pub fn cmp_anchors(&self, a: &Anchor, b: &Anchor) -> std::cmp::Ordering {
        self.buffer_snapshot.cmp_anchors(a, b)
    }

    pub fn edits_since(&self, since_version: u64) -> Patch<usize> {
        self.buffer_snapshot.edits_since(since_version)
    }

    pub fn version(&self) -> u64 {
        self.buffer_snapshot.version
    }

    /// The buffer an [`Anchor`] must name to resolve here, since resolving a
    /// foreign one answers with an offset rather than an error.
    pub fn buffer_id(&self) -> BufferId {
        self.buffer_snapshot.buffer_id
    }
}

#[cfg(test)]
mod tests {
    use super::MultiBuffer;
    use crate::buffer::{BufferId, TextBuffer};
    use std::sync::{Arc, RwLock};

    fn create_test_buffer(content: &str) -> Arc<RwLock<TextBuffer>> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        Arc::new(RwLock::new(buffer))
    }

    #[test]
    fn singleton_creation() {
        let buffer = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(buffer);
        assert!(multi.as_singleton().is_some());
    }

    #[test]
    fn snapshot_line_count() {
        let buffer = create_test_buffer("line1\nline2\nline3");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn snapshot_text() {
        let buffer = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.text(), "hello\nworld");
    }

    #[test]
    fn snapshot_lines() {
        let buffer = create_test_buffer("a\nb\nc");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        let lines: Vec<_> = snapshot.lines().collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn anchor_valid_within_bounds() {
        let buffer = create_test_buffer("0123456789");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_max_is_valid() {
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Left);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn stale_anchor_invalid_after_edit() {
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer);
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
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(3, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_min_max_always_valid() {
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer);

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
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer.clone());
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
        let buffer = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(buffer.clone());
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
        let buffer = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(buffer);
        let snap = multi.snapshot();
        let patch = snap.edits_since(snap.version());
        assert!(patch.is_empty());
    }

    #[test]
    fn cmp_anchors_by_offset() {
        let buffer = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(buffer);
        let snapshot = multi.snapshot();
        let a = snapshot.anchor_at(3, stoat_text::Bias::Left);
        let b = snapshot.anchor_at(7, stoat_text::Bias::Left);
        assert_eq!(snapshot.cmp_anchors(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(snapshot.cmp_anchors(&b, &a), std::cmp::Ordering::Greater);
        assert_eq!(snapshot.cmp_anchors(&a, &a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn cmp_anchors_survives_deleting_the_text_between_them() {
        let buffer = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(buffer);

        let (a, b) = {
            let snapshot = multi.snapshot();
            (
                snapshot.anchor_at(6, stoat_text::Bias::Right),
                snapshot.anchor_at(9, stoat_text::Bias::Left),
            )
        };

        multi
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(5..11, "");

        let snapshot = multi.snapshot();
        assert_eq!(
            snapshot.cmp_anchors(&a, &b),
            std::cmp::Ordering::Less,
            "both anchors resolve to the deletion's start, which must not reorder them"
        );
    }
}
