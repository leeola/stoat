use super::{
    fold_map::{FoldChunks, FoldOffset, FoldPoint, FoldSnapshot},
    highlights::{Chunk, HighlightEndpoint},
};
use std::{
    borrow::Cow,
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

    pub fn set_tab_size(&mut self, size: NonZeroU32) {
        self.tab_size = size;
    }

    pub fn sync(
        &mut self,
        fold_snapshot: Arc<FoldSnapshot>,
        fold_edits: Patch<u32>,
    ) -> (TabSnapshot, Patch<u32>) {
        let tab_size = self.tab_size.get();

        let expanded_edits = if fold_edits.is_empty() {
            fold_edits
        } else {
            let mut expanded = Vec::new();
            for edit in fold_edits.into_iter() {
                // Widening by the row past the end is conservative rather than
                // required. Expansion restarts at every newline, so a row's tab
                // stops depend on nothing before its own start, and fold edits
                // arrive as whole rows, which leaves no way for an edit to move
                // the stops of a row outside it.
                //
                // Kept because it costs one row per same-size edit, where
                // dropping it would stake correctness on edits staying
                // row-granular for good.
                let mut new_end = edit.new.end;
                let old_rows = edit.old.end - edit.old.start;
                let new_rows = edit.new.end - edit.new.start;
                if old_rows == new_rows && edit.new.end < fold_snapshot.line_count() {
                    let has_tab = fold_snapshot
                        .fold_line_chars(edit.new.end)
                        .any(|ch| ch == '\t');
                    if has_tab {
                        new_end = new_end.max(edit.new.end + 1);
                    }
                }

                // The extra row goes on both sides. Taking the old end from the
                // new range's length instead would restate the old range in new
                // coordinates, which reads as "no rows were added" to every
                // layer below and leaves them unable to grow.
                let extension = new_end - edit.new.end;
                let new_edit = stoat_text::patch::Edit {
                    old: edit.old.start..edit.old.end + extension,
                    new: edit.new.start..new_end,
                };

                if let Some(last) = expanded.last_mut() {
                    let last: &mut stoat_text::patch::Edit<u32> = last;
                    if new_edit.old.start <= last.old.end {
                        last.old.end = last.old.end.max(new_edit.old.end);
                        last.new.end = last.new.end.max(new_edit.new.end);
                        continue;
                    }
                }
                expanded.push(new_edit);
            }
            Patch::new(expanded)
        };

        self.version += 1;
        let snapshot = TabSnapshot {
            fold_snapshot,
            tab_size,
            max_expansion_column: MAX_EXPANSION_COLUMN,
            version: self.version,
        };
        (snapshot, expanded_edits)
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

    pub fn tab_point_to_fold_point_detailed(
        &self,
        tab_point: TabPoint,
        bias: Bias,
    ) -> (FoldPoint, u32, u32) {
        let chars = self.fold_snapshot.fold_line_chars(tab_point.row());
        let (fold_column, expanded_char_column, to_next_stop) = collapse_column_detailed(
            chars,
            tab_point.column(),
            self.tab_size,
            bias,
            self.max_expansion_column,
        );
        (
            FoldPoint::new(tab_point.row(), fold_column),
            expanded_char_column,
            to_next_stop,
        )
    }

    pub fn line_len(&self, fold_row: u32) -> u32 {
        let fold_line_len = self.fold_snapshot.line_len(fold_row);
        expand_column(
            self.fold_snapshot.fold_line_chars(fold_row),
            fold_line_len,
            self.tab_size,
            self.max_expansion_column,
        )
    }

    /// Move `point` to the nearest position a caret can occupy, preferring the
    /// side `bias` names.
    ///
    /// A column inside a tab's expansion names no character, so it resolves to
    /// the tab's own column or to the stop it runs to. Descends into the fold
    /// layer, which places a column inside a fold placeholder or an inlay on
    /// one of its edges. Stays on the row it clamped `point` to.
    pub fn clip_point(&self, point: TabPoint, bias: Bias) -> TabPoint {
        let max_row = self.line_count().saturating_sub(1);
        let row = point.row().min(max_row);
        let max_col = self.line_len(row);
        let col = point.column().min(max_col);

        let fold_point = self.to_fold_point(TabPoint::new(row, col), bias);
        self.to_tab_point(self.fold_snapshot.clip_point(fold_point, bias))
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
            // A zero-width char has no column of its own. It sits at the column
            // the char before it ended on, which at a wrap break is this row's
            // end, so cutting there would hand it to the next row while its base
            // char stays here. The next row's start test then skips it as
            // already behind, and it is painted nowhere.
            let past_end = |end: u32| {
                if width == 0 {
                    column > end
                } else {
                    column >= end
                }
            };
            if let Some(end) = end_col
                && past_end(end)
            {
                break;
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
            pending_offset: 0,
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
    /// Byte cursor into [`Self::pending`]'s text for a borrowed chunk being
    /// split across successive tabs, so each split emits a subslice rather than
    /// reallocating the remainder. Always `0` for owned (block-row) pending.
    pending_offset: usize,
    display_column: u32,
    tab_size: u32,
    max_expansion_column: u32,
}

impl<'a> Iterator for TabChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        if self.pending.is_none() {
            self.pending = self.fold_chunks.next();
            self.pending.as_ref()?;
            self.pending_offset = 0;
        }

        // A borrowed chunk's text points into the rope data (lifetime `'a`), so
        // subslices stay valid after `pending` is cleared and the tab split emits
        // them without copying. An owned block-row chunk's text is held by
        // `pending`, so subslices would borrow `self`, not `'a`. It keeps the
        // allocating path.
        let borrowed = match &self.pending.as_ref().expect("refilled above").text {
            Cow::Borrowed(text) => Some(*text),
            Cow::Owned(_) => None,
        };
        match borrowed {
            Some(text) => self.next_borrowed(text),
            None => self.next_owned(),
        }
    }
}

impl<'a> TabChunks<'a> {
    /// Emit the next chunk from a borrowed pending whose full text is `text`,
    /// splitting at the tab under [`Self::pending_offset`].
    ///
    /// The whole remainder, the run before a tab, and the tab's spaces are all
    /// borrowed slices, so a chunk with several tabs never recopies its tail.
    fn next_borrowed(&mut self, text: &'a str) -> Option<Chunk<'a>> {
        let remaining = &text[self.pending_offset..];
        match remaining.find('\t') {
            Some(0) => {
                let spaces = self.tab_width();
                self.display_column += spaces;
                self.pending_offset += 1;
                if self.pending_offset >= text.len() {
                    self.pending = None;
                    self.pending_offset = 0;
                }
                Some(Chunk {
                    text: Cow::Borrowed(tab_spaces_slice(spaces)),
                    is_tab: true,
                    highlight_style: None,
                    ..Default::default()
                })
            },
            Some(idx) => {
                let metadata =
                    clone_chunk_metadata(self.pending.as_ref().expect("borrowed pending"));
                let prefix = &remaining[..idx];
                advance_display_column(prefix, &mut self.display_column);
                self.pending_offset += idx;
                Some(Chunk {
                    text: Cow::Borrowed(prefix),
                    ..metadata
                })
            },
            None => {
                let metadata =
                    clone_chunk_metadata(self.pending.as_ref().expect("borrowed pending"));
                advance_display_column(remaining, &mut self.display_column);
                self.pending = None;
                self.pending_offset = 0;
                Some(Chunk {
                    text: Cow::Borrowed(remaining),
                    ..metadata
                })
            },
        }
    }

    /// Emit the next chunk from an owned pending (a block row), reallocating the
    /// remainder around each tab. Owned text is held by `pending` and cannot be
    /// borrowed for `'a`.
    fn next_owned(&mut self) -> Option<Chunk<'a>> {
        let pending = self.pending.take().expect("owned pending");
        self.pending_offset = 0;

        let text: &str = pending.text.as_ref();
        match text.find('\t') {
            None => {
                advance_display_column(&pending.text, &mut self.display_column);
                Some(pending)
            },
            Some(0) => {
                let spaces = self.tab_width();
                self.display_column += spaces;
                let rest = text[1..].to_string();
                let metadata = clone_chunk_metadata(&pending);
                if !rest.is_empty() {
                    self.pending = Some(Chunk {
                        text: Cow::Owned(rest),
                        ..metadata
                    });
                }
                Some(Chunk {
                    text: Cow::Borrowed(tab_spaces_slice(spaces)),
                    is_tab: true,
                    highlight_style: None,
                    ..Default::default()
                })
            },
            Some(idx) => {
                let prefix = text[..idx].to_string();
                let rest = text[idx..].to_string();
                let metadata = clone_chunk_metadata(&pending);
                self.pending = Some(Chunk {
                    text: Cow::Owned(rest),
                    highlight_style: metadata.highlight_style.clone(),
                    is_tab: metadata.is_tab,
                    is_inlay: metadata.is_inlay,
                    inlay_kind: metadata.inlay_kind,
                    diagnostic_severity: metadata.diagnostic_severity,
                    renderer: metadata.renderer.clone(),
                });
                advance_display_column(&prefix, &mut self.display_column);
                Some(Chunk {
                    text: Cow::Owned(prefix),
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
const TAB_SPACES: &str = "                                                                                                                                                                                                                                                                                                                                ";

fn tab_spaces_slice(width: u32) -> &'static str {
    let len = (width as usize).min(TAB_SPACES.len());
    &TAB_SPACES[..len]
}

/// A run of exactly `width` spaces, borrowed from the shared static.
///
/// `None` once `width` outruns the static, leaving the caller to allocate.
/// Unlike [`tab_spaces_slice`] this never shortens the run, since a caller
/// padding to a column needs the width it asked for or nothing.
pub(super) fn spaces(width: u32) -> Option<&'static str> {
    TAB_SPACES.get(..width as usize)
}

/// Advance an expanded (tab-expanded, display-width) column past `ch`.
///
/// A tab jumps to the next `tab_size` stop until `max_expansion_column`, past
/// which it counts as a single column. Any other char adds its display width.
/// Shared with the render painter so its fast path and the display map's column
/// math cannot drift.
pub(crate) fn advance_column_for_char(
    expanded: &mut u32,
    ch: char,
    tab_size: u32,
    max_expansion_column: u32,
) {
    if ch == '\t' {
        if *expanded >= max_expansion_column {
            *expanded += 1;
        } else {
            *expanded += tab_size - (*expanded % tab_size);
        }
    } else {
        *expanded += super::display_width(ch);
    }
}

pub(super) fn expand_column(
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
        advance_column_for_char(&mut expanded, ch, tab_size, max_expansion_column);
        byte_idx += ch.len_utf8() as u32;
    }
    expanded
}

pub(super) fn collapse_column(
    chars: impl Iterator<Item = char>,
    tab_column: u32,
    tab_size: u32,
    bias: Bias,
    max_expansion_column: u32,
) -> u32 {
    let mut chars = chars.peekable();
    let mut expanded = 0u32;
    let mut fold_col = 0u32;
    let mut last_char_byte_len = 0u32;
    while let Some(&ch) = chars.peek() {
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
        chars.next();
    }
    if bias == Bias::Left && expanded > tab_column {
        fold_col = fold_col.saturating_sub(last_char_byte_len);
    } else {
        fold_col += trailing_zero_width_bytes(&mut chars, fold_col);
    }
    fold_col
}

/// Bytes of the zero-width characters `chars` is sitting in front of, which
/// belong to the character the walk just consumed rather than to the position
/// after it.
///
/// A mark occupying no cells is reached only after the walk has already covered
/// the column it was asked for, so it is left in front of unless it is taken
/// here, and the column returned would name a byte inside a character.
///
/// `consumed` is how many bytes the walk took. Nothing means the position is at
/// the line start, where a mark has no character before it to continue and
/// stands as one itself, so it is left alone.
fn trailing_zero_width_bytes(
    chars: &mut std::iter::Peekable<impl Iterator<Item = char>>,
    consumed: u32,
) -> u32 {
    if consumed == 0 {
        return 0;
    }
    let mut bytes = 0u32;
    while let Some(&ch) = chars.peek() {
        if ch == '\t' || super::display_width(ch) != 0 {
            break;
        }
        bytes += ch.len_utf8() as u32;
        chars.next();
    }
    bytes
}

fn collapse_column_detailed(
    chars: impl Iterator<Item = char>,
    tab_column: u32,
    tab_size: u32,
    bias: Bias,
    max_expansion_column: u32,
) -> (u32, u32, u32) {
    let mut chars = chars.peekable();
    let mut expanded = 0u32;
    let mut fold_col = 0u32;
    let mut last_char_byte_len = 0u32;
    let mut last_char_width = 0u32;
    while let Some(&ch) = chars.peek() {
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
        last_char_width = char_width;
        fold_col += last_char_byte_len;
        chars.next();
    }
    if bias == Bias::Left && expanded > tab_column {
        fold_col = fold_col.saturating_sub(last_char_byte_len);
        expanded -= last_char_width;
    } else {
        // Only the byte column moves. The characters taken occupy no cells, so
        // the expanded column and the distance to the next tab stop that
        // follows from it are the same as before.
        fold_col += trailing_zero_width_bytes(&mut chars, fold_col);
    }
    let to_next_stop = if expanded >= max_expansion_column {
        1
    } else {
        tab_size - (expanded % tab_size)
    };
    (fold_col, expanded, to_next_stop)
}

#[cfg(test)]
mod tests {
    use super::{TabMap, TabPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            fold_map::{FoldMap, FoldPoint},
            inlay_map::InlayMap,
        },
        multi_buffer::MultiBuffer,
    };
    use std::{
        num::NonZeroU32,
        sync::{Arc, RwLock},
    };
    use stoat_text::{patch::Patch, Bias, Point};

    /// Byte column a display column converts to, both ways round, since a
    /// position on a character boundary must not depend on the bias.
    fn byte_column(line: &str, display_column: u32) -> u32 {
        let left = super::collapse_column(line.chars(), display_column, 4, Bias::Left, u32::MAX);
        let right = super::collapse_column(line.chars(), display_column, 4, Bias::Right, u32::MAX);
        assert_eq!(left, right, "bias must not matter on a boundary");
        left
    }

    #[test]
    fn a_display_column_converts_past_a_combining_mark() {
        // Characters begin at bytes 0, 3 and 4. Column 1 is after the accented
        // one, so it converts to 3 rather than to the 1 that sits between the
        // letter and its accent.
        assert_eq!(byte_column("e\u{301}x", 0), 0);
        assert_eq!(byte_column("e\u{301}x", 1), 3);
        assert_eq!(byte_column("e\u{301}x", 2), 4);
    }

    #[test]
    fn a_mark_later_in_the_line_converts_the_same_way() {
        // Characters at 0, 1, 4 and 5.
        assert_eq!(byte_column("ab\u{301}c", 1), 1);
        assert_eq!(byte_column("ab\u{301}c", 2), 4);
        assert_eq!(byte_column("ab\u{301}c", 3), 5);
    }

    #[test]
    fn a_leading_mark_keeps_the_line_start() {
        // With nothing before it the mark is a character in its own right, so
        // column 0 is byte 0 rather than past it.
        assert_eq!(byte_column("\u{301}x", 0), 0);
        assert_eq!(byte_column("\u{301}x", 1), 3);
    }

    #[test]
    fn a_left_bias_stepping_back_off_a_wide_character_stays_put() {
        // The wide character spans bytes 1..4 and occupies columns 1 and 2,
        // with the mark continuing it. A left bias landing in its middle steps
        // back before it, to byte 1. The mark belongs to the character stepped
        // over rather than to the position, so taking it would land byte 3,
        // which is inside the wide character itself.
        let line = "a\u{4e00}\u{301}x";
        assert_eq!(
            super::collapse_column(line.chars(), 2, 4, Bias::Left, u32::MAX),
            1,
        );
    }

    #[test]
    fn the_detailed_conversion_moves_only_its_byte_column() {
        // Same answer as the plain one, and the cell bookkeeping beside it is
        // untouched, the characters taken occupying no cells. One column in,
        // three cells short of the next four-wide tab stop.
        assert_eq!(
            super::collapse_column_detailed("e\u{301}x".chars(), 1, 4, Bias::Left, u32::MAX),
            (3, 1, 3),
        );
    }

    fn make_snapshot(content: &str) -> super::TabSnapshot {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(NonZeroU32::new(4).unwrap());
        let (snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        snapshot
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
        assert_eq!(
            snap.clip_point(TabPoint::new(5, 0), Bias::Left),
            TabPoint::new(1, 0)
        );
        assert_eq!(
            snap.clip_point(TabPoint::new(0, 100), Bias::Left),
            TabPoint::new(0, 5)
        );
    }

    /// Sync widens each incoming edit to the rows whose tab stops it moved.
    /// Editing inside a tabbed line moves that line's own stops, so its row has
    /// to come back in the emitted edit. The row past the edit comes with it
    /// when the edit changed no line count, since its stops move too and it sits
    /// outside the edit that reports them.
    #[test]
    fn an_edit_inside_a_tabbed_line_invalidates_that_row() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "\tone\n\ttwo\n\tthree\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(multi_buffer.snapshot());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(NonZeroU32::new(4).unwrap());

        let before = multi_buffer.snapshot();
        tab_map.sync(
            fold_map
                .sync(
                    inlay_map.sync(before.clone(), &Patch::empty()).0,
                    &Patch::empty(),
                )
                .0,
            Patch::empty(),
        );

        // Insert inside row 1, between its tab and its text.
        let at = before.rope().point_to_offset(Point::new(1, 1));
        shared.write().expect("poisoned").edit(at..at, "X");

        let after = multi_buffer.snapshot();
        let buffer_edits = after.edits_since(before.version());
        let (inlay_snapshot, inlay_edits) = inlay_map.sync(after, &buffer_edits);
        let (fold_snapshot, fold_edits) = fold_map.sync(inlay_snapshot, &inlay_edits);
        let (_, tab_edits) = tab_map.sync(fold_snapshot, fold_edits);

        let covered: Vec<(u32, u32)> = tab_edits
            .edits()
            .iter()
            .map(|edit| (edit.new.start, edit.new.end))
            .collect();
        assert!(
            covered.iter().any(|&(start, end)| start <= 1 && 1 < end),
            "row 1 carries the edit, so its expansion must be rebuilt: {covered:?}",
        );
    }

    /// An emitted edit states its old range in old coordinates.
    ///
    /// Every layer below reads the row delta off the difference between the two
    /// ranges. Restating the old range in new coordinates makes an insertion read
    /// as a same-size replacement, and the layers below then never grow.
    #[test]
    fn an_inserted_row_keeps_its_delta_in_the_emitted_edit() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "one\ntwo\nthree\n");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(multi_buffer.snapshot());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(NonZeroU32::new(4).unwrap());

        let before = multi_buffer.snapshot();
        tab_map.sync(
            fold_map
                .sync(
                    inlay_map.sync(before.clone(), &Patch::empty()).0,
                    &Patch::empty(),
                )
                .0,
            Patch::empty(),
        );

        shared.write().expect("poisoned").edit(0..0, "a\nb\nc\n");

        let after = multi_buffer.snapshot();
        let buffer_edits = after.edits_since(before.version());
        let (inlay_snapshot, inlay_edits) = inlay_map.sync(after, &buffer_edits);
        let (fold_snapshot, fold_edits) = fold_map.sync(inlay_snapshot, &inlay_edits);
        let fold_delta: i64 = fold_edits
            .edits()
            .iter()
            .map(|e| (e.new.end - e.new.start) as i64 - (e.old.end - e.old.start) as i64)
            .sum();
        let (_, tab_edits) = tab_map.sync(fold_snapshot, fold_edits);

        let tab_delta: i64 = tab_edits
            .edits()
            .iter()
            .map(|e| (e.new.end - e.new.start) as i64 - (e.old.end - e.old.start) as i64)
            .sum();
        assert_eq!(
            (fold_delta, tab_delta),
            (3, 3),
            "three inserted rows survive into the tab edit: {:?}",
            tab_edits
                .edits()
                .iter()
                .map(|e| (e.old.clone(), e.new.clone()))
                .collect::<Vec<_>>(),
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
}
