use crate::git::BufferDiff;
use std::{ops::Range, sync::Arc};
use stoat_text::{Anchor, Bias, Point, Rope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(u64);

impl BufferId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EditRecord {
    pub(crate) version: usize,
    pub(crate) range: Range<usize>,
    pub(crate) new_len: usize,
}

pub struct TextBuffer {
    pub rope: Rope,
    pub dirty: bool,
    pub diff: Option<BufferDiff>,
    pub version: usize,
    pub(crate) edit_log: Arc<Vec<EditRecord>>,
    compacted_to: usize,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            dirty: false,
            diff: None,
            version: 0,
            edit_log: Arc::new(Vec::new()),
            compacted_to: 0,
        }
    }

    pub fn edit(&mut self, range: Range<usize>, text: &str) {
        Arc::make_mut(&mut self.edit_log).push(EditRecord {
            version: self.version,
            range: range.clone(),
            new_len: text.len(),
        });
        self.rope.replace(range, text);
        self.version += 1;
        self.dirty = true;
    }

    pub fn anchor_at(&self, offset: usize, bias: Bias) -> Anchor {
        Anchor {
            version: self.version,
            offset: offset.min(self.rope.len()),
            bias,
        }
    }

    pub fn resolve_anchor(&self, anchor: &Anchor) -> usize {
        resolve_anchor_in_log(&self.edit_log, anchor, self.rope.len())
    }

    pub fn point_for_anchor(&self, anchor: &Anchor) -> Point {
        self.rope.offset_to_point(self.resolve_anchor(anchor))
    }

    pub fn compact_edit_log(&mut self, watermark: usize) {
        if watermark <= self.compacted_to {
            return;
        }
        Arc::make_mut(&mut self.edit_log).retain(|r| r.version >= watermark);
        self.compacted_to = watermark;
    }

    pub fn compacted_to(&self) -> usize {
        self.compacted_to
    }

    pub fn line_count(&self) -> u32 {
        self.rope.max_point().row + 1
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn resolve_anchor_in_log(
    edit_log: &[EditRecord],
    anchor: &Anchor,
    max_len: usize,
) -> usize {
    if anchor.offset == usize::MAX {
        return max_len;
    }
    let mut offset = anchor.offset;
    let start_idx = edit_log.partition_point(|r| r.version < anchor.version);
    for record in &edit_log[start_idx..] {
        let new_end = record.range.start + record.new_len;
        if offset <= record.range.start {
            if offset == record.range.start
                && record.range.start == record.range.end
                && anchor.bias == Bias::Right
            {
                offset = new_end;
            }
        } else if offset >= record.range.end {
            let old_len = record.range.end - record.range.start;
            offset = offset - old_len + record.new_len;
        } else {
            match anchor.bias {
                Bias::Left => offset = record.range.start,
                Bias::Right => offset = new_end,
            }
        }
    }
    offset.min(max_len)
}

pub(crate) fn resolve_anchors_batch(
    edit_log: &[EditRecord],
    anchors: &[Anchor],
    max_len: usize,
) -> Vec<usize> {
    let mut offsets: Vec<usize> = anchors.iter().map(|a| a.offset).collect();
    let min_version = anchors
        .iter()
        .map(|a| a.version)
        .min()
        .unwrap_or(usize::MAX);
    let start_idx = edit_log.partition_point(|r| r.version < min_version);
    for record in &edit_log[start_idx..] {
        for (i, anchor) in anchors.iter().enumerate() {
            if record.version < anchor.version {
                continue;
            }
            if anchors[i].offset == usize::MAX {
                continue;
            }
            let new_end = record.range.start + record.new_len;
            if offsets[i] <= record.range.start {
                if offsets[i] == record.range.start
                    && record.range.start == record.range.end
                    && anchor.bias == Bias::Right
                {
                    offsets[i] = new_end;
                }
            } else if offsets[i] >= record.range.end {
                let old_len = record.range.end - record.range.start;
                offsets[i] = offsets[i] - old_len + record.new_len;
            } else {
                match anchor.bias {
                    Bias::Left => offsets[i] = record.range.start,
                    Bias::Right => offsets[i] = new_end,
                }
            }
        }
    }
    for offset in &mut offsets {
        *offset = (*offset).min(max_len);
    }
    offsets
}

pub type SharedBuffer = Arc<std::sync::RwLock<TextBuffer>>;

#[cfg(test)]
mod tests {
    use super::{resolve_anchors_batch, TextBuffer};
    use stoat_text::Bias;

    fn buf(content: &str) -> TextBuffer {
        let mut b = TextBuffer::new();
        b.rope.push(content);
        b
    }

    #[test]
    fn anchor_insert_before() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Right);
        b.edit(0..0, "XX");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_insert_after() {
        let mut b = buf("hello");
        let a = b.anchor_at(2, Bias::Right);
        b.edit(4..4, "XX");
        assert_eq!(b.resolve_anchor(&a), 2);
    }

    #[test]
    fn anchor_delete_before() {
        let mut b = buf("hello");
        let a = b.anchor_at(4, Bias::Right);
        b.edit(0..2, "");
        assert_eq!(b.resolve_anchor(&a), 2);
    }

    #[test]
    fn anchor_bias_left_at_insertion() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Left);
        b.edit(3..3, "XX");
        assert_eq!(b.resolve_anchor(&a), 3);
    }

    #[test]
    fn anchor_bias_right_at_insertion() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Right);
        b.edit(3..3, "XX");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_within_deleted_range_left() {
        let mut b = buf("hello world");
        let a = b.anchor_at(7, Bias::Left);
        b.edit(5..11, "");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_within_deleted_range_right() {
        let mut b = buf("hello world");
        let a = b.anchor_at(7, Bias::Right);
        b.edit(5..11, "");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_multiple_edits() {
        let mut b = buf("abcdef");
        let a = b.anchor_at(4, Bias::Right);
        b.edit(0..0, "XX");
        b.edit(3..5, "Y");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_min_max() {
        let mut b = buf("hello");
        let min = stoat_text::Anchor::min();
        let max = stoat_text::Anchor::max();
        assert_eq!(b.resolve_anchor(&min), 0);
        assert_eq!(b.resolve_anchor(&max), 5);
        b.edit(5..5, " world");
        assert_eq!(b.resolve_anchor(&min), 0);
        assert_eq!(b.resolve_anchor(&max), 11);
    }

    #[test]
    fn batch_resolve() {
        let mut b = buf("hello");
        let a1 = b.anchor_at(1, Bias::Right);
        let a2 = b.anchor_at(3, Bias::Right);
        b.edit(0..0, "XX");
        let offsets = resolve_anchors_batch(&b.edit_log, &[a1, a2], b.rope.len());
        assert_eq!(offsets, vec![3, 5]);
    }

    #[test]
    fn batch_resolve_anchor_max_with_multiple_edits() {
        let mut b = buf("hello world");
        let a1 = b.anchor_at(3, Bias::Right);
        let a_max = stoat_text::Anchor::max();
        b.edit(0..2, "");
        b.edit(4..6, "");
        let offsets = resolve_anchors_batch(&b.edit_log, &[a1, a_max], b.rope.len());
        assert_eq!(offsets[0], b.resolve_anchor(&a1));
        assert_eq!(offsets[1], b.resolve_anchor(&a_max));
        assert_eq!(offsets[1], b.rope.len());
    }

    #[test]
    fn compact_edit_log() {
        let mut b = buf("hello");
        let a1 = b.anchor_at(2, Bias::Right);
        for _ in 0..100 {
            b.edit(0..0, "X");
        }
        let a2 = b.anchor_at(50, Bias::Right);
        b.compact_edit_log(a1.version);
        assert!(b.edit_log.len() < 102);
        assert_eq!(b.resolve_anchor(&a2), 50);
    }

    #[test]
    fn point_for_anchor_multiline() {
        let mut b = buf("hello\nworld");
        let a = b.anchor_at(8, Bias::Right);
        b.edit(0..0, "XX");
        let point = b.point_for_anchor(&a);
        assert_eq!(point, stoat_text::Point::new(1, 2));
    }

    #[test]
    fn resolve_skips_early_records() {
        let mut b = buf("hello");
        for _ in 0..100 {
            b.edit(0..0, "X");
        }
        let a = b.anchor_at(50, Bias::Right);
        b.edit(0..0, "Y");
        assert_eq!(b.resolve_anchor(&a), 51);
    }
}
