use super::{
    fold_map::{FoldChunks, FoldOffset, FoldPoint, FoldSnapshot},
    highlights::{Chunk, HighlightEndpoint},
};
use std::{
    borrow::Cow,
    mem,
    num::NonZeroU32,
    ops::{Deref, Range},
    sync::Arc,
};
use stoat_text::{patch::Patch, Bias};

const MAX_EXPANSION_COLUMN: u32 = 256;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabPoint(pub FoldPoint);

impl TabPoint {
    pub fn zero() -> Self {
        Self(FoldPoint::new(0, 0))
    }

    pub fn new(row: u32, column: u32) -> Self {
        Self(FoldPoint::new(row, column))
    }

    pub fn row(&self) -> u32 {
        self.0.row()
    }

    pub fn column(&self) -> u32 {
        self.0.column()
    }
}

impl From<FoldPoint> for TabPoint {
    fn from(point: FoldPoint) -> Self {
        Self(point)
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabRow(pub u32);

pub struct TabMap {
    tab_size: NonZeroU32,
    version: usize,
}

impl TabMap {
    pub fn new(tab_size: NonZeroU32) -> Self {
        Self {
            tab_size,
            version: 0,
        }
    }

    /// Advances to `fold_snapshot` and returns the tab-row patch for the
    /// wrap layer.
    ///
    /// The patch is `fold_edits` unchanged. Tab expansion only widens columns
    /// within a row, never row counts, so a row-granular fold edit already
    /// covers every row whose expansion can change and the translation is the
    /// identity. Extending an edit to a neighboring row would only
    /// over-invalidate the wrap layer.
    pub fn sync(
        &mut self,
        fold_snapshot: Arc<FoldSnapshot>,
        fold_edits: Patch<u32>,
    ) -> (TabSnapshot, Patch<u32>) {
        self.version += 1;
        let snapshot = TabSnapshot {
            fold_snapshot,
            tab_size: self.tab_size.get(),
            max_expansion_column: MAX_EXPANSION_COLUMN,
            version: self.version,
        };
        (snapshot, fold_edits)
    }
}

#[derive(Clone)]
pub struct TabSnapshot {
    fold_snapshot: Arc<FoldSnapshot>,
    tab_size: u32,
    max_expansion_column: u32,
    version: usize,
}

impl Deref for TabSnapshot {
    type Target = FoldSnapshot;
    fn deref(&self) -> &FoldSnapshot {
        &self.fold_snapshot
    }
}

impl TabSnapshot {
    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        &self.fold_snapshot
    }

    pub fn tab_size(&self) -> u32 {
        self.tab_size
    }

    pub fn max_expansion_column(&self) -> u32 {
        self.max_expansion_column
    }

    pub fn version(&self) -> usize {
        self.version
    }

    pub fn to_tab_point(&self, fold_point: FoldPoint) -> TabPoint {
        let chars = self.fold_snapshot.fold_line_chars(fold_point.row());
        let expanded_column = expand_column(
            chars,
            fold_point.column(),
            self.tab_size,
            self.max_expansion_column,
        );
        TabPoint::new(fold_point.row(), expanded_column)
    }

    pub fn to_fold_point(&self, tab_point: TabPoint, bias: Bias) -> FoldPoint {
        let chars = self.fold_snapshot.fold_line_chars(tab_point.row());
        let fold_column = collapse_column(
            chars,
            tab_point.column(),
            self.tab_size,
            bias,
            self.max_expansion_column,
        );
        FoldPoint::new(tab_point.row(), fold_column)
    }

    pub fn line_len(&self, fold_row: u32) -> u32 {
        let mut expanded = 0u32;
        for ch in self.fold_snapshot.fold_line_chars(fold_row) {
            if ch == '\t' {
                if expanded >= self.max_expansion_column {
                    expanded += 1;
                } else {
                    expanded += self.tab_size - (expanded % self.tab_size);
                }
            } else {
                expanded += super::display_width(ch);
            }
        }
        expanded
    }

    pub fn clip_point(&self, point: TabPoint, bias: Bias) -> TabPoint {
        let fold_point = self.to_fold_point(point, bias);
        let clipped = self.fold_snapshot.clip_point(fold_point, bias);
        self.to_tab_point(clipped)
    }

    pub fn write_expand_line(&self, buf: &mut String, fold_row: u32) {
        let mut column = 0u32;
        for ch in self.fold_snapshot.fold_line_chars(fold_row) {
            if ch == '\t' {
                let width = if column >= self.max_expansion_column {
                    1
                } else {
                    self.tab_size - (column % self.tab_size)
                };
                for _ in 0..width {
                    buf.push(' ');
                }
                column += width;
            } else {
                buf.push(ch);
                column += super::display_width(ch);
            }
        }
    }

    pub fn expand_line(&self, fold_row: u32) -> String {
        let mut result = String::new();
        self.write_expand_line(&mut result, fold_row);
        result
    }

    pub fn write_expand_line_range(
        &self,
        buf: &mut String,
        fold_row: u32,
        start_col: u32,
        end_col: Option<u32>,
    ) {
        let mut column = 0u32;
        for ch in self.fold_snapshot.fold_line_chars(fold_row) {
            let width = if ch == '\t' {
                if column >= self.max_expansion_column {
                    1
                } else {
                    self.tab_size - (column % self.tab_size)
                }
            } else {
                super::display_width(ch)
            };

            let next_column = column + width;

            if next_column <= start_col {
                column = next_column;
                continue;
            }
            if let Some(end) = end_col {
                if column >= end {
                    break;
                }
            }

            if ch == '\t' {
                let visible_start = start_col.max(column);
                let visible_end = end_col.map_or(next_column, |e| e.min(next_column));
                for _ in 0..(visible_end - visible_start) {
                    buf.push(' ');
                }
            } else {
                buf.push(ch);
            }
            column = next_column;
        }
    }

    pub fn expand_line_range(&self, fold_row: u32, start_col: u32, end_col: Option<u32>) -> String {
        let mut result = String::new();
        self.write_expand_line_range(&mut result, fold_row, start_col, end_col);
        result
    }

    pub fn line_count(&self) -> u32 {
        self.fold_snapshot.line_count()
    }

    /// Stream [`Chunk`]s covering a fold-offset range with tabs expanded.
    ///
    /// `start_column` is the display column at `range.start`; pass 0 when
    /// starting at a row boundary (typical editor use). Tabs encountered in
    /// the chunk stream are emitted as separate unstyled chunks tagged with
    /// [`Chunk::is_tab`], sized to advance the running display column to the
    /// next multiple of [`TabSnapshot::tab_size`] (clamped to
    /// [`TabSnapshot::max_expansion_column`]).
    ///
    /// Newlines reset the display column to 0. The caller is responsible for
    /// ensuring the starting column is accurate.
    pub fn chunks<'a>(
        &'a self,
        range: Range<FoldOffset>,
        start_column: u32,
        endpoints: Arc<[HighlightEndpoint]>,
    ) -> TabChunks<'a> {
        TabChunks {
            fold_chunks: self.fold_snapshot.chunks(range, endpoints),
            pending: None,
            display_column: start_column,
            tab_size: self.tab_size,
            max_expansion_column: self.max_expansion_column,
        }
    }
}

/// Iterator returned by [`TabSnapshot::chunks`]. Splits incoming chunks at
/// tab characters and emits tab-expansion chunks interleaved with the
/// preserved-style runs.
pub struct TabChunks<'a> {
    fold_chunks: FoldChunks<'a>,
    pending: Option<Chunk<'a>>,
    display_column: u32,
    tab_size: u32,
    max_expansion_column: u32,
}

impl<'a> Iterator for TabChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        // Refill pending if needed.
        if self.pending.is_none() {
            self.pending = self.fold_chunks.next();
            self.pending.as_ref()?;
        }
        // Take ownership of pending so we can mutate self freely.
        let mut pending = self.pending.take().expect("refilled above");

        match pending.text.find('\t') {
            None => {
                // No tab in this chunk. Emit whole chunk, advance column.
                advance_display_column(&pending.text, &mut self.display_column);
                Some(pending)
            },
            Some(0) => {
                // Chunk starts with a tab. Emit the expansion chunk and push
                // the remainder back into pending.
                let spaces = self.tab_width();
                self.display_column += spaces;
                let rest = slice_cow_from(mem::take(&mut pending.text), 1);
                if !rest.is_empty() {
                    self.pending = Some(Chunk {
                        text: rest,
                        ..clone_chunk_metadata(&pending)
                    });
                }
                // Inherit the surrounding chunk's style/diagnostic/inlay
                // metadata so a styled range spanning the tab keeps its
                // appearance over the expanded cells.
                Some(Chunk {
                    text: Cow::Borrowed(tab_spaces_slice(spaces)),
                    is_tab: true,
                    ..clone_chunk_metadata(&pending)
                })
            },
            Some(idx) => {
                // Emit prefix before the tab; push the tab-plus-suffix back
                // into pending so the next call processes them.
                let (prefix, rest) = split_cow_at(mem::take(&mut pending.text), idx);
                let metadata = clone_chunk_metadata(&pending);
                self.pending = Some(Chunk {
                    text: rest,
                    highlight_style: metadata.highlight_style.clone(),
                    is_tab: metadata.is_tab,
                    is_inlay: metadata.is_inlay,
                    inlay_kind: metadata.inlay_kind,
                    diagnostic_severity: metadata.diagnostic_severity,
                    renderer: metadata.renderer.clone(),
                });
                advance_display_column(&prefix, &mut self.display_column);
                Some(Chunk {
                    text: prefix,
                    ..metadata
                })
            },
        }
    }
}

fn clone_chunk_metadata<'a>(chunk: &Chunk<'a>) -> Chunk<'a> {
    Chunk {
        text: Cow::Borrowed(""),
        highlight_style: chunk.highlight_style.clone(),
        is_tab: chunk.is_tab,
        is_inlay: chunk.is_inlay,
        inlay_kind: chunk.inlay_kind,
        diagnostic_severity: chunk.diagnostic_severity,
        renderer: chunk.renderer.clone(),
    }
}

/// Slice a chunk's text from byte `start` to its end. A borrowed chunk keeps
/// its `'a` lifetime so the suffix is a free re-slice; an owned chunk falls
/// back to allocating the remainder.
fn slice_cow_from(cow: Cow<'_, str>, start: usize) -> Cow<'_, str> {
    match cow {
        Cow::Borrowed(s) => Cow::Borrowed(&s[start..]),
        Cow::Owned(s) => Cow::Owned(s[start..].to_string()),
    }
}

/// Split a chunk's text at byte `idx` into prefix and suffix. A borrowed chunk
/// re-slices both halves for free; an owned chunk allocates each half.
fn split_cow_at(cow: Cow<'_, str>, idx: usize) -> (Cow<'_, str>, Cow<'_, str>) {
    match cow {
        Cow::Borrowed(s) => (Cow::Borrowed(&s[..idx]), Cow::Borrowed(&s[idx..])),
        Cow::Owned(s) => (
            Cow::Owned(s[..idx].to_string()),
            Cow::Owned(s[idx..].to_string()),
        ),
    }
}

impl TabChunks<'_> {
    fn tab_width(&self) -> u32 {
        if self.display_column >= self.max_expansion_column {
            1
        } else {
            self.tab_size - (self.display_column % self.tab_size)
        }
    }
}

fn advance_display_column(text: &str, column: &mut u32) {
    for ch in text.chars() {
        if ch == '\n' {
            *column = 0;
        } else {
            *column += super::display_width(ch);
        }
    }
}

// A static slice of spaces long enough to cover any tab expansion
// (up to `MAX_EXPANSION_COLUMN` + tab_size slop). The returned subslice
// is always a valid UTF-8 slice of ASCII spaces.
const TAB_SPACES: &str =
    "                                                                                                                                                                                                                                                                                                                                ";

fn tab_spaces_slice(width: u32) -> &'static str {
    let len = (width as usize).min(TAB_SPACES.len());
    &TAB_SPACES[..len]
}

fn expand_column(
    chars: impl Iterator<Item = char>,
    fold_column: u32,
    tab_size: u32,
    max_expansion_column: u32,
) -> u32 {
    let mut expanded = 0u32;
    let mut byte_idx = 0u32;
    for ch in chars {
        if byte_idx >= fold_column {
            break;
        }
        if ch == '\t' {
            if expanded >= max_expansion_column {
                expanded += 1;
            } else {
                expanded += tab_size - (expanded % tab_size);
            }
        } else {
            expanded += super::display_width(ch);
        }
        byte_idx += ch.len_utf8() as u32;
    }
    expanded
}

fn collapse_column(
    chars: impl Iterator<Item = char>,
    tab_column: u32,
    tab_size: u32,
    bias: Bias,
    max_expansion_column: u32,
) -> u32 {
    let mut expanded = 0u32;
    let mut fold_col = 0u32;
    let mut last_char_byte_len = 0u32;
    for ch in chars {
        if expanded >= tab_column {
            break;
        }
        let char_width = if ch == '\t' {
            if expanded >= max_expansion_column {
                1
            } else {
                tab_size - (expanded % tab_size)
            }
        } else {
            super::display_width(ch)
        };
        expanded += char_width;
        last_char_byte_len = ch.len_utf8() as u32;
        fold_col += last_char_byte_len;
    }
    if bias == Bias::Left && expanded > tab_column {
        fold_col = fold_col.saturating_sub(last_char_byte_len);
    }
    fold_col
}

#[cfg(test)]
mod tests {
    use super::{TabMap, TabPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            display_width,
            fold_map::{FoldMap, FoldPoint, FoldSnapshot},
            inlay_map::InlayMap,
        },
        multi_buffer::MultiBuffer,
    };
    use std::{
        num::NonZeroU32,
        sync::{Arc, RwLock},
    };
    use stoat_text::{
        patch::{Edit, Patch},
        Bias,
    };

    fn make_fold_snapshot(content: &str) -> Arc<FoldSnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        fold_snapshot
    }

    fn make_snapshot(content: &str) -> super::TabSnapshot {
        let mut tab_map = TabMap::new(NonZeroU32::new(4).unwrap());
        let (snapshot, _) = tab_map.sync(make_fold_snapshot(content), Patch::empty());
        snapshot
    }

    #[test]
    fn sync_passes_fold_row_patch_through_unchanged() {
        let fold_snapshot = make_fold_snapshot("abc\n\tdef");
        let mut tab_map = TabMap::new(NonZeroU32::new(4).unwrap());
        let fold_edits = Patch::new(vec![Edit {
            old: 0..1,
            new: 0..1,
        }]);

        let (snapshot, tab_edits) = tab_map.sync(fold_snapshot, fold_edits.clone());

        assert_eq!(
            tab_edits, fold_edits,
            "tab translation must be the identity"
        );
        assert_eq!(snapshot.version(), 1);
    }

    #[test]
    fn no_tabs_passthrough() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 3)), TabPoint::new(0, 3));
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 3), Bias::Left),
            FoldPoint::new(0, 3)
        );
        assert_eq!(snap.line_len(0), 5);
    }

    #[test]
    fn single_tab_expansion() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 0)), TabPoint::new(0, 0));
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 1)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn tab_after_text() {
        let snap = make_snapshot("ab\tcd");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 2)), TabPoint::new(0, 2));
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 3)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 6);
    }

    #[test]
    fn multiple_tabs() {
        let snap = make_snapshot("\t\tx");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 2)), TabPoint::new(0, 8));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn column_roundtrip() {
        let snap = make_snapshot("a\tb\tc");
        for col in 0..5u32 {
            let tab = snap.to_tab_point(FoldPoint::new(0, col));
            let back = snap.to_fold_point(tab, Bias::Left);
            assert_eq!(
                back,
                FoldPoint::new(0, col),
                "roundtrip failed for col {col}"
            );
        }
    }

    #[test]
    fn multiline() {
        let snap = make_snapshot("no tabs\n\tindented");
        assert_eq!(snap.line_len(0), 7);
        assert_eq!(snap.line_len(1), 12);
        assert_eq!(snap.to_tab_point(FoldPoint::new(1, 1)), TabPoint::new(1, 4));
    }

    #[test]
    fn bias_inside_tab() {
        let snap = make_snapshot("\thello");
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 2), Bias::Left),
            FoldPoint::new(0, 0)
        );
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 2), Bias::Right),
            FoldPoint::new(0, 1)
        );
    }

    #[test]
    fn clip_point_clamps() {
        let snap = make_snapshot("hello\nhi");
        // A past-end row clamps to the buffer's end, not the start of the last
        // line: the fold round-trip resolves an out-of-range point to max_point.
        assert_eq!(
            snap.clip_point(TabPoint::new(5, 0), Bias::Left),
            TabPoint::new(1, 2)
        );
        assert_eq!(
            snap.clip_point(TabPoint::new(0, 100), Bias::Left),
            TabPoint::new(0, 5)
        );
    }

    #[test]
    fn clip_point_snaps_inside_tab() {
        // "\tx": the tab expands to 4 cells (columns 0..4), x at column 4.
        let snap = make_snapshot("\tx");
        // A column inside the tab expansion snaps to a cell boundary -- Left to
        // the tab's start, Right to the cell after it -- rather than surviving.
        assert_eq!(
            snap.clip_point(TabPoint::new(0, 2), Bias::Left),
            TabPoint::new(0, 0)
        );
        assert_eq!(
            snap.clip_point(TabPoint::new(0, 2), Bias::Right),
            TabPoint::new(0, 4)
        );
    }

    #[test]
    fn expand_line_range_full_line() {
        let snap = make_snapshot("\thello\tworld");
        let full = snap.expand_line(0);
        let ranged = snap.expand_line_range(0, 0, None);
        assert_eq!(ranged, full);
    }

    #[test]
    fn expand_line_range_with_tabs() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.expand_line_range(0, 0, Some(4)), "    ");
        assert_eq!(snap.expand_line_range(0, 4, None), "hello");
    }

    #[test]
    fn expand_line_range_partial_tab() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.expand_line_range(0, 2, Some(4)), "  ");
    }

    #[test]
    fn expand_line_range_cjk() {
        let snap = make_snapshot("\u{4e16}\u{754c}hello");
        // Each CJK char is 2 display columns wide
        assert_eq!(snap.expand_line_range(0, 0, Some(4)), "\u{4e16}\u{754c}");
        assert_eq!(snap.expand_line_range(0, 4, None), "hello");
    }

    #[test]
    fn cjk_collapse_bias_left() {
        let snap = make_snapshot("\u{4e16}hello");
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 1), Bias::Left),
            FoldPoint::new(0, 0),
        );
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 2), Bias::Left),
            FoldPoint::new(0, 3),
        );
    }

    #[test]
    fn cjk_roundtrip() {
        let snap = make_snapshot("\u{4e16}\u{754c}hello");
        for col in [0u32, 3, 6, 7, 8, 9, 10, 11] {
            let tab = snap.to_tab_point(FoldPoint::new(0, col));
            let back = snap.to_fold_point(tab, Bias::Left);
            assert_eq!(
                back,
                FoldPoint::new(0, col),
                "roundtrip failed for col {col}"
            );
        }
    }

    #[test]
    fn max_expansion_column_caps_tabs() {
        let mut content = "x".repeat(260);
        content.push('\t');
        content.push('y');
        let snap = make_snapshot(&content);
        assert_eq!(
            snap.to_tab_point(FoldPoint::new(0, 261)),
            TabPoint::new(0, 261)
        );
        assert_eq!(snap.line_len(0), 262);
    }

    #[test]
    fn write_expand_line_matches_expand_line() {
        let snap = make_snapshot("\thello\tworld\nno tabs\n\t\tx");
        for row in 0..snap.line_count() {
            let expected = snap.expand_line(row);
            let mut buf = String::new();
            snap.write_expand_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    #[test]
    fn chunks_no_tabs_forwards_fold_chunks() {
        use crate::display_map::fold_map::FoldOffset;

        let snap = make_snapshot("hello world");
        let end = snap.fold_snapshot().len();
        let text: String = snap
            .chunks(FoldOffset(0)..end, 0, Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn chunks_single_leading_tab_expands() {
        use crate::display_map::fold_map::FoldOffset;

        let snap = make_snapshot("\thello");
        let end = snap.fold_snapshot().len();
        let chunks: Vec<_> = snap
            .chunks(FoldOffset(0)..end, 0, Arc::from(Vec::new()))
            .collect();

        // A leading tab with tab_size=4 expands to 4 spaces.
        let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(text, "    hello");

        // The tab must be marked as a distinct is_tab chunk.
        let tab_chunks: Vec<_> = chunks.iter().filter(|c| c.is_tab).collect();
        assert_eq!(tab_chunks.len(), 1);
        assert_eq!(tab_chunks[0].text.as_ref(), "    ");
    }

    #[test]
    fn chunks_tab_in_middle_splits_chunk() {
        use crate::display_map::fold_map::FoldOffset;

        let snap = make_snapshot("ab\tcd");
        let end = snap.fold_snapshot().len();
        let chunks: Vec<_> = snap
            .chunks(FoldOffset(0)..end, 0, Arc::from(Vec::new()))
            .collect();

        // Expected expansion: "ab" + "  " (tab at col 2, expands to col 4) + "cd"
        let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(text, "ab  cd");

        let tab_chunks: Vec<_> = chunks.iter().filter(|c| c.is_tab).collect();
        assert_eq!(tab_chunks.len(), 1);
        assert_eq!(tab_chunks[0].text.as_ref(), "  ");
    }

    #[test]
    fn tab_chunk_inherits_surrounding_style() {
        use crate::{
            display_map::{
                fold_map::FoldOffset,
                highlights::{
                    create_highlight_endpoints, HighlightKey, HighlightLayer, HighlightStyle,
                    OffsetAnchorResolver,
                },
            },
            style::Color,
        };
        use std::collections::HashMap;
        use stoat_text::Anchor;

        let snap = make_snapshot("\thello");
        let end = snap.fold_snapshot().len();

        // Style the whole "\thello" range red so it spans the tab.
        let red = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let mut highlights_map = HashMap::new();
        let key = HighlightKey::new(HighlightLayer::SyntaxToken, 0);
        let mk_anchor = |offset: usize| Anchor {
            timestamp: 0,
            offset: offset as u32,
            bias: Bias::Left,
            buffer_id: None,
        };
        highlights_map.insert(key, Arc::new((red, vec![mk_anchor(0)..mk_anchor(6)])));
        let highlights = Arc::new(highlights_map);
        let endpoints: Arc<[_]> = Arc::from(create_highlight_endpoints(
            &(0..6),
            &highlights,
            None,
            None,
            &OffsetAnchorResolver,
        ));

        let chunks: Vec<_> = snap.chunks(FoldOffset(0)..end, 0, endpoints).collect();
        let tab = chunks.iter().find(|c| c.is_tab).expect("tab chunk present");
        assert_eq!(
            tab.highlight_style.as_ref().and_then(|s| s.foreground),
            Some(Color::Red),
            "tab-expansion chunk inherits the surrounding highlight"
        );
    }

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self((seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03).max(1))
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u32) -> u32 {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as u32
            }
        }
    }

    fn random_tab_content(rng: &mut Rng, len: usize) -> String {
        // narrow ASCII, space, tab, a 3-byte wide CJK char, a 4-byte wide
        // char, and newline (escaped so the source stays ASCII).
        const ALPHABET: &[char] = &['a', 'b', ' ', '\t', '\u{4E16}', '\u{1F680}', '\n'];
        (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len() as u32) as usize])
            .collect()
    }

    /// Independent char-walk expansion: the tab-stop and cap rule implemented
    /// from the spec, reusing the shared `display_width` primitive. Returns the
    /// expanded (tab) column at byte offset `up_to_byte` of `chars`.
    fn ref_expand(chars: &[char], up_to_byte: u32, tab_size: u32, max: u32) -> u32 {
        let mut expanded = 0u32;
        let mut byte = 0u32;
        for &ch in chars {
            if byte >= up_to_byte {
                break;
            }
            expanded += if ch == '\t' {
                if expanded >= max {
                    1
                } else {
                    tab_size - expanded % tab_size
                }
            } else {
                display_width(ch)
            };
            byte += ch.len_utf8() as u32;
        }
        expanded
    }

    #[test]
    fn random_tabs() {
        for seed in 0..64u64 {
            let mut rng = Rng::new(seed);
            let tab_size = 1 + rng.below(8);
            let content_len = rng.below(40) as usize;
            let content = random_tab_content(&mut rng, content_len);

            let mut tab_map = TabMap::new(NonZeroU32::new(tab_size).unwrap());
            let (snapshot, _) = tab_map.sync(make_fold_snapshot(&content), Patch::empty());
            let max = snapshot.max_expansion_column();
            let ctx = || format!("seed {seed} tab_size {tab_size} content {content:?}");

            for row in 0..snapshot.line_count() {
                let chars: Vec<char> = snapshot.fold_snapshot().fold_line_chars(row).collect();
                let mut byte = 0u32;
                let mut boundaries = vec![0u32];
                for ch in &chars {
                    byte += ch.len_utf8() as u32;
                    boundaries.push(byte);
                }
                let total = *boundaries.last().expect("includes 0");

                assert_eq!(
                    snapshot.line_len(row),
                    ref_expand(&chars, total, tab_size, max),
                    "line_len row {row} {}",
                    ctx()
                );

                let expanded = snapshot.expand_line(row);
                assert!(
                    !expanded.contains('\t'),
                    "expand_line keeps a tab, row {row} {}",
                    ctx()
                );
                assert_eq!(
                    expanded.chars().map(display_width).sum::<u32>(),
                    snapshot.line_len(row),
                    "expand_line width row {row} {}",
                    ctx()
                );

                for &p in &boundaries {
                    let tab_col = snapshot.to_tab_point(FoldPoint::new(row, p)).column();
                    assert_eq!(
                        tab_col,
                        ref_expand(&chars, p, tab_size, max),
                        "to_tab_point row {row} byte {p} {}",
                        ctx()
                    );
                    assert_eq!(
                        snapshot
                            .to_fold_point(TabPoint::new(row, tab_col), Bias::Left)
                            .column(),
                        p,
                        "expand/collapse round trip row {row} byte {p} {}",
                        ctx()
                    );
                }

                for tab_col in 0..=snapshot.line_len(row) {
                    let clipped = snapshot.clip_point(TabPoint::new(row, tab_col), Bias::Left);
                    assert_eq!(
                        snapshot.clip_point(clipped, Bias::Left),
                        clipped,
                        "clip_point idempotent row {row} col {tab_col} {}",
                        ctx()
                    );
                }
            }
        }
    }

    #[test]
    fn tab_expansion_caps_at_max_expansion_column() {
        // 255 narrow chars reach column 255; the wide char straddles column 256
        // (255 -> 257); each following tab sits past the cap and expands to 1.
        let mut content = "a".repeat(255);
        content.push('\u{4E16}');
        content.push_str("\t\t");
        let snapshot = make_snapshot(&content);
        let max = snapshot.max_expansion_column();
        let chars: Vec<char> = snapshot.fold_snapshot().fold_line_chars(0).collect();
        let total: u32 = chars.iter().map(|c| c.len_utf8() as u32).sum();

        assert_eq!(
            snapshot.line_len(0),
            ref_expand(&chars, total, snapshot.tab_size(), max),
            "line_len matches the reference across the expansion cap"
        );
        assert_eq!(
            snapshot.line_len(0),
            259,
            "255 narrow + wide(2) + two capped tabs(1 each)"
        );
    }
}
