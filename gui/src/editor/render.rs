use crate::{diagnostics::DiagnosticSet, git::blame};
use gpui::{
    div, px, rgb, Div, FontStyle, FontWeight, Hsla, ParentElement, Pixels, SharedString,
    StrikethroughStyle, Styled, StyledText, UnderlineStyle,
};
use lsp_types::DiagnosticSeverity;
use ratatui::style::{Color, Modifier, Style as RatatuiStyle};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};
use stoat::{
    display_map::{Block, BlockContext, BlockId, HighlightStyle as StoatHighlightStyle},
    host::BlameLine,
    review::MoveProvenance,
    review_session::ChunkStatus,
    BlockRowKind, DisplayPoint, DisplaySnapshot, MultiBufferSnapshot,
};
use stoat_text::{Anchor, Bias, Selection};

const NAMED_COLOR_HEX: [u32; 16] = [
    0x000000, // 0 Black
    0xcd0000, // 1 Red
    0x00cd00, // 2 Green
    0xcdcd00, // 3 Yellow
    0x0000ee, // 4 Blue
    0xcd00cd, // 5 Magenta
    0x00cdcd, // 6 Cyan
    0xe5e5e5, // 7 Gray (aka White-7)
    0x7f7f7f, // 8 DarkGray (aka Bright Black)
    0xff0000, // 9 LightRed
    0x00ff00, // 10 LightGreen
    0xffff00, // 11 LightYellow
    0x5c5cff, // 12 LightBlue
    0xff00ff, // 13 LightMagenta
    0x00ffff, // 14 LightCyan
    0xffffff, // 15 White
];

pub(crate) fn ratatui_color_to_hsla(color: Color) -> Option<Hsla> {
    let hex = match color {
        Color::Reset => return None,
        Color::Black => NAMED_COLOR_HEX[0],
        Color::Red => NAMED_COLOR_HEX[1],
        Color::Green => NAMED_COLOR_HEX[2],
        Color::Yellow => NAMED_COLOR_HEX[3],
        Color::Blue => NAMED_COLOR_HEX[4],
        Color::Magenta => NAMED_COLOR_HEX[5],
        Color::Cyan => NAMED_COLOR_HEX[6],
        Color::Gray => NAMED_COLOR_HEX[7],
        Color::DarkGray => NAMED_COLOR_HEX[8],
        Color::LightRed => NAMED_COLOR_HEX[9],
        Color::LightGreen => NAMED_COLOR_HEX[10],
        Color::LightYellow => NAMED_COLOR_HEX[11],
        Color::LightBlue => NAMED_COLOR_HEX[12],
        Color::LightMagenta => NAMED_COLOR_HEX[13],
        Color::LightCyan => NAMED_COLOR_HEX[14],
        Color::White => NAMED_COLOR_HEX[15],
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Indexed(n) => indexed_color_hex(n),
    };
    Some(rgb(hex).into())
}

fn indexed_color_hex(n: u8) -> u32 {
    if (n as usize) < NAMED_COLOR_HEX.len() {
        return NAMED_COLOR_HEX[n as usize];
    }
    if n >= 232 {
        let level = 8u32 + 10u32 * u32::from(n - 232);
        return (level << 16) | (level << 8) | level;
    }
    let offset = u32::from(n - 16);
    let r = offset / 36;
    let g = (offset / 6) % 6;
    let b = offset % 6;
    let channel = |v: u32| if v == 0 { 0 } else { 55 + 40 * v };
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

pub(crate) fn convert_highlight_style(src: &StoatHighlightStyle) -> gpui::HighlightStyle {
    gpui::HighlightStyle {
        color: src.foreground.and_then(ratatui_color_to_hsla),
        background_color: src.background.and_then(ratatui_color_to_hsla),
        font_weight: src.bold.and_then(|b| b.then_some(FontWeight::BOLD)),
        font_style: src.italic.and_then(|b| b.then_some(FontStyle::Italic)),
        underline: src.underline.and_then(|b| {
            b.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            })
        }),
        strikethrough: src.strikethrough.and_then(|b| {
            b.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            })
        }),
        fade_out: None,
    }
}

pub(crate) struct RenderedRow {
    pub text: SharedString,
    pub runs: Vec<(Range<usize>, gpui::HighlightStyle)>,
}

pub(crate) fn build_rendered_rows(
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
) -> Vec<RenderedRow> {
    let count = range.end.saturating_sub(range.start) as usize;
    let mut texts: Vec<String> = vec![String::new(); count];
    let mut runs: Vec<Vec<(Range<usize>, gpui::HighlightStyle)>> = vec![Vec::new(); count];

    let mut current = 0usize;
    for chunk in snapshot.highlighted_chunks(range.clone()) {
        let style = chunk.highlight_style.as_ref().map(convert_highlight_style);
        let mut remaining: &str = chunk.text.as_ref();
        while !remaining.is_empty() && current < count {
            match remaining.find('\n') {
                Some(nl) => {
                    append_run(
                        &mut texts[current],
                        &mut runs[current],
                        &remaining[..nl],
                        style,
                    );
                    current += 1;
                    remaining = &remaining[nl + 1..];
                },
                None => {
                    append_run(&mut texts[current], &mut runs[current], remaining, style);
                    remaining = "";
                },
            }
        }
    }

    let buffer_snapshot = snapshot.buffer_snapshot();
    for idx in 0..count {
        let display_row = range.start + idx as u32;
        if let BlockRowKind::Block { block, line_index } = snapshot.classify_row(display_row) {
            let ctx = block_context_for(block, display_row, buffer_snapshot);
            let lines = block.render_lines(&ctx);
            let Some(line) = lines.into_iter().nth(line_index as usize) else {
                texts[idx].clear();
                runs[idx].clear();
                continue;
            };
            let fallback = block_fallback_color(block);
            let mut row_text = String::new();
            let mut row_runs: Vec<(Range<usize>, gpui::HighlightStyle)> = Vec::new();
            for span in line.spans {
                if span.content.is_empty() {
                    continue;
                }
                let start = row_text.len();
                row_text.push_str(span.content.as_ref());
                let end = row_text.len();
                row_runs.push((start..end, convert_block_span_style(&span.style, fallback)));
            }
            texts[idx] = row_text;
            runs[idx] = row_runs;
        }
    }

    apply_token_overlay(&texts, &mut runs, snapshot, range);

    texts
        .into_iter()
        .zip(runs)
        .map(|(text, runs)| RenderedRow {
            text: SharedString::from(text),
            runs,
        })
        .collect()
}

fn block_context_for<'a>(
    block: &Block,
    anchor_row: u32,
    buffer_snapshot: &'a MultiBufferSnapshot,
) -> BlockContext<'a> {
    let block_id = match block {
        Block::Custom(b) => BlockId::Custom(b.id),
        Block::FoldedBuffer { first_excerpt, .. } => BlockId::FoldedBuffer(first_excerpt.id),
        Block::ExcerptBoundary { excerpt, .. } => BlockId::ExcerptBoundary(excerpt.id),
        Block::BufferHeader { excerpt, .. } => BlockId::BufferHeader(excerpt.id),
        Block::Spacer { id, .. } => BlockId::Spacer(*id),
    };
    let diff_status = match block {
        Block::Custom(b) => b.diff_status,
        _ => None,
    };
    BlockContext {
        block_id,
        max_width: 256,
        height: block.height(),
        selected: false,
        anchor_row,
        diff_status,
        buffer_snapshot,
    }
}

fn convert_ratatui_style(style: &RatatuiStyle) -> gpui::HighlightStyle {
    let modifier = style.add_modifier;
    gpui::HighlightStyle {
        color: style.fg.and_then(ratatui_color_to_hsla),
        background_color: style.bg.and_then(ratatui_color_to_hsla),
        font_weight: modifier
            .contains(Modifier::BOLD)
            .then_some(FontWeight::BOLD),
        font_style: modifier
            .contains(Modifier::ITALIC)
            .then_some(FontStyle::Italic),
        underline: modifier
            .contains(Modifier::UNDERLINED)
            .then(|| UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            }),
        strikethrough: modifier
            .contains(Modifier::CROSSED_OUT)
            .then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
        fade_out: None,
    }
}

fn convert_block_span_style(style: &RatatuiStyle, fallback_color: u32) -> gpui::HighlightStyle {
    let mut converted = convert_ratatui_style(style);
    if converted.color.is_none() {
        converted.color = Some(rgb(fallback_color).into());
    }
    converted
}

fn block_fallback_color(block: &Block) -> u32 {
    let Block::Custom(custom) = block else {
        return BLOCK_TEXT_HEX;
    };
    match custom.diff_status {
        Some(stoat::DiffHunkStatus::Deleted) | Some(stoat::DiffHunkStatus::Modified) => {
            DIFF_DELETED_HEX
        },
        Some(stoat::DiffHunkStatus::Moved) => DIFF_MOVED_HEX,
        _ => BLOCK_TEXT_HEX,
    }
}

fn token_overlay_style(color_hex: u32) -> gpui::HighlightStyle {
    gpui::HighlightStyle {
        underline: Some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(rgb(color_hex).into()),
            wavy: false,
        }),
        ..Default::default()
    }
}

fn buffer_side_token_color(kind: &stoat::ChangeKind, hunk_status: stoat::DiffStatus) -> u32 {
    if matches!(kind, stoat::ChangeKind::Moved) {
        return DIFF_MOVED_HEX;
    }
    match hunk_status {
        stoat::DiffStatus::Added => DIFF_ADDED_HEX,
        stoat::DiffStatus::Moved => DIFF_MOVED_HEX,
        stoat::DiffStatus::StagedAdded => DIFF_STAGED_ADDED_HEX,
        stoat::DiffStatus::StagedModified => DIFF_STAGED_MODIFIED_HEX,
        stoat::DiffStatus::StagedDeleted => DIFF_STAGED_DELETED_HEX,
        stoat::DiffStatus::CommittedAdded => DIFF_COMMITTED_ADDED_HEX,
        stoat::DiffStatus::CommittedModified => DIFF_COMMITTED_MODIFIED_HEX,
        stoat::DiffStatus::CommittedDeleted => DIFF_COMMITTED_DELETED_HEX,
        _ => DIFF_MODIFIED_HEX,
    }
}

fn base_side_token_color(kind: &stoat::ChangeKind) -> u32 {
    match kind {
        stoat::ChangeKind::Moved => DIFF_MOVED_HEX,
        _ => DIFF_DELETED_HEX,
    }
}

/// Byte range (in the full `base_text`) of the line at
/// `line_index` within `hunk.base_byte_range`'s slice. Matches the
/// line splitting used by `BlockProperties::from_text` (which calls
/// `content.lines()` and so produces line ranges without the
/// trailing `\n`).
fn block_line_base_range(
    hunk: &stoat::DiffHunk,
    base_text: &str,
    line_index: usize,
) -> Option<Range<usize>> {
    let content = base_text.get(hunk.base_byte_range.clone())?;
    let mut count = 0usize;
    let mut start = 0usize;
    for (i, ch) in content.char_indices() {
        if ch == '\n' {
            if count == line_index {
                let mut end = i;
                if end > start && content.as_bytes().get(end - 1) == Some(&b'\r') {
                    end -= 1;
                }
                return Some(
                    (hunk.base_byte_range.start + start)..(hunk.base_byte_range.start + end),
                );
            }
            count += 1;
            start = i + 1;
        }
    }
    if count == line_index && start < content.len() {
        return Some(
            (hunk.base_byte_range.start + start)..(hunk.base_byte_range.start + content.len()),
        );
    }
    None
}

fn apply_token_overlay(
    texts: &[String],
    runs: &mut [Vec<(Range<usize>, gpui::HighlightStyle)>],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
) {
    let Some(diff_map) = snapshot.diff_map() else {
        return;
    };
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    for (idx, row_runs) in runs.iter_mut().enumerate() {
        let display_row = range.start + idx as u32;
        match snapshot.classify_row(display_row) {
            BlockRowKind::Block { block, line_index } => {
                let Block::Custom(custom) = block else {
                    continue;
                };
                let Some(status) = custom.diff_status else {
                    continue;
                };
                if !matches!(
                    status,
                    stoat::DiffHunkStatus::Deleted | stoat::DiffHunkStatus::Modified
                ) {
                    continue;
                }
                let placement_line = match custom.placement {
                    stoat::display_map::BlockPlacement::Below(n) => n,
                    _ => continue,
                };
                // `deleted_blocks()` uses
                // `placement = Below(buffer_start_line.saturating_sub(1))`,
                // so a placement_line of 0 may belong to a hunk starting
                // at either row 0 or row 1.
                let hunks = diff_map.hunks_in_range(placement_line..placement_line + 2);
                let Some(hunk) = hunks.into_iter().find(|h| {
                    (h.buffer_start_line == placement_line + 1
                        || h.buffer_start_line == placement_line)
                        && matches!(
                            h.status,
                            stoat::DiffHunkStatus::Deleted | stoat::DiffHunkStatus::Modified
                        )
                        && !h.base_byte_range.is_empty()
                }) else {
                    continue;
                };
                let Some(detail) = hunk.token_detail.as_ref() else {
                    continue;
                };
                let Some(base_text) = diff_map.base_text() else {
                    continue;
                };
                let Some(line_range) = block_line_base_range(hunk, base_text, line_index as usize)
                else {
                    continue;
                };
                let row_len = texts[idx].len();
                for span in &detail.base_spans {
                    let s = span.byte_range.start.max(line_range.start);
                    let e = span.byte_range.end.min(line_range.end);
                    if s >= e {
                        continue;
                    }
                    let local_start = s - line_range.start;
                    let local_end = (e - line_range.start).min(row_len);
                    if local_start >= local_end {
                        continue;
                    }
                    let color = base_side_token_color(&span.kind);
                    row_runs.push((local_start..local_end, token_overlay_style(color)));
                }
            },
            _ => {
                let Some(buffer_point) =
                    snapshot.display_to_buffer(DisplayPoint::new(display_row, 0))
                else {
                    continue;
                };
                let buffer_row = buffer_point.row;
                let Some(detail) = diff_map.token_detail_for_line(buffer_row) else {
                    continue;
                };
                let row_start = rope.point_to_offset(stoat_text::Point::new(buffer_row, 0));
                let row_len = texts[idx].len();
                let row_end = row_start + row_len;
                let hunk_status = diff_map.status_for_line(buffer_row);
                for span in &detail.buffer_spans {
                    let s = span.byte_range.start.max(row_start);
                    let e = span.byte_range.end.min(row_end);
                    if s >= e {
                        continue;
                    }
                    let local_start = s - row_start;
                    let local_end = e - row_start;
                    let color = buffer_side_token_color(&span.kind, hunk_status);
                    row_runs.push((local_start..local_end, token_overlay_style(color)));
                }
            },
        }
    }
}

/// Overlay a background highlight on every visible regex match of
/// `query` in the active buffer text. Pushed runs paint a
/// `background_color: Some(highlight_color)` band behind matched
/// characters; matches that span multiple display rows or cross
/// the visible row range are split per-row and clamped to the
/// visible window. Invalid regex and empty queries are silent
/// no-ops, matching the TUI's
/// [`stoat::action_handlers::search`] behaviour.
pub(crate) fn apply_search_overlay(
    rows: &mut [RenderedRow],
    byte_maps: &[RowByteMap],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
    regex: &regex::Regex,
    highlight_color: Hsla,
) {
    if rows.is_empty() {
        return;
    }
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let visible = visible_byte_range(snapshot, &range, rope.len());
    if visible.is_empty() {
        return;
    }
    let slice: String = rope.chunks_in_range(visible.clone()).collect();
    let style = gpui::HighlightStyle {
        background_color: Some(highlight_color),
        ..Default::default()
    };
    for m in regex.find_iter(&slice) {
        if m.start() == m.end() {
            continue;
        }
        let match_range = (visible.start + m.start())..(visible.start + m.end());
        push_syntax_runs(rows, byte_maps, match_range, style);
    }
}

/// Overlay tree-sitter syntax-highlight runs on every visible row
/// in `rows`. Walks the multi-layer [`stoat_language::SyntaxSnapshot`]
/// for captures whose byte range intersects the visible buffer
/// window, resolves each capture's [`stoat_language::HighlightId`]
/// via the originating language's pre-installed
/// [`stoat_language::HighlightMap`] (seeded once by
/// [`crate::globals::seed_language_highlight_maps`]), looks the id
/// up in `styles.id_for_highlight`, and pushes a run per visible
/// character. Captures that resolve to
/// [`stoat_language::HighlightId::DEFAULT`] are skipped (the
/// capture has no theme entry and the renderer leaves it unstyled).
///
/// Multi-row captures clamp per-row; captures whose row falls
/// outside `range` are skipped.
pub(crate) fn apply_syntax_overlay(
    rows: &mut [RenderedRow],
    byte_maps: &[RowByteMap],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
    syntax_snapshot: &stoat_language::SyntaxSnapshot,
    styles: &stoat::display_map::syntax_theme::SyntaxStyles,
) {
    if rows.is_empty() || syntax_snapshot.is_empty() {
        return;
    }
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let visible_byte_range = visible_byte_range(snapshot, &range, rope.len());
    if visible_byte_range.is_empty() {
        return;
    }
    let captures =
        syntax_snapshot.captures(visible_byte_range, rope, |lang| Some(&lang.highlight_query));
    for capture in captures {
        let highlight_id = capture.language.highlight_map().get(capture.index);
        let Some(style_id) = styles.id_for_highlight(highlight_id) else {
            continue;
        };
        let stoat_style = &styles.interner[style_id];
        let gpui_style = convert_highlight_style(stoat_style);
        let raw_range = capture.node.byte_range();
        let node_range = rope.clip_offset(raw_range.start, Bias::Left)
            ..rope.clip_offset(raw_range.end, Bias::Left);
        push_syntax_runs(rows, byte_maps, node_range, gpui_style);
    }
}

/// Per-display-row lookup table that converts between rope byte
/// offsets and `RenderedRow.text` byte offsets without paying a
/// per-character `offset_to_point` / `buffer_to_display` /
/// `column_to_byte_offset` chain.
///
/// Built once at the top of [`crate::editor::Editor::render_visible_rows`]
/// via [`build_row_byte_maps`] and shared across the syntax and search
/// overlays. Indexed by `display_row - range.start` to align with the
/// rendered rows slice.
#[derive(Debug, Clone)]
pub(crate) struct RowByteMap {
    /// Rope byte offset of the first character on this display row.
    pub buffer_start_offset: usize,
    /// One past the rope byte offset of the last character that
    /// belongs to this row (the newline, if present, falls in the gap
    /// between this row's `buffer_end_offset` and the next row's
    /// `buffer_start_offset`). Set to `rope.len()` for the trailing
    /// row.
    pub buffer_end_offset: usize,
    /// Byte offsets within `RenderedRow.text` for each character on
    /// the row. Length = char_count + 1; the trailing entry is
    /// `row_text.len()` so adjacent entries bracket a character's
    /// bytes.
    pub columns: Box<[usize]>,
    /// Rope byte offsets for each character on the row, parallel to
    /// `columns`. The trailing entry is `buffer_end_offset`.
    /// `partition_point(|&o| o <= rope_offset) - 1` returns the
    /// column index for any rope offset within the row's range.
    pub rope_offsets: Box<[usize]>,
}

pub(crate) fn build_row_byte_maps(
    rows: &[RenderedRow],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
) -> Vec<RowByteMap> {
    let rope = snapshot.buffer_snapshot().rope();
    let rope_len = rope.len();
    let mut maps = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        let display_row = range.start + idx as u32;
        let buffer_start_offset = snapshot
            .display_to_buffer(DisplayPoint::new(display_row, 0))
            .map(|p| rope.point_to_offset(p))
            .unwrap_or(rope_len)
            .min(rope_len);
        let next_row_start = snapshot
            .display_to_buffer(DisplayPoint::new(display_row + 1, 0))
            .map(|p| rope.point_to_offset(p))
            .unwrap_or(rope_len)
            .min(rope_len);
        let row_text: &str = row.text.as_ref();
        let mut columns: Vec<usize> = row_text.char_indices().map(|(b, _)| b).collect();
        columns.push(row_text.len());
        let mut rope_offsets: Vec<usize> = Vec::with_capacity(columns.len());
        let mut chars = rope.chars_at(buffer_start_offset);
        let mut offset = buffer_start_offset;
        rope_offsets.push(offset);
        // Walk one character per display column; stop at the next
        // row's start (which may be a newline boundary or the rope
        // end). The newline char itself is consumed but not assigned
        // a column.
        while rope_offsets.len() < columns.len() && offset < next_row_start {
            let Some(ch) = chars.next() else {
                break;
            };
            let ch_len = ch.len_utf8();
            offset += ch_len;
            if ch == '\n' {
                break;
            }
            rope_offsets.push(offset);
        }
        // Pad with the row-end offset if the rope ran out before we
        // filled the column array (defensive: keeps the parallel
        // structure intact so `partition_point` lookups stay safe).
        while rope_offsets.len() < columns.len() {
            rope_offsets.push(offset);
        }
        maps.push(RowByteMap {
            buffer_start_offset,
            buffer_end_offset: offset,
            columns: columns.into_boxed_slice(),
            rope_offsets: rope_offsets.into_boxed_slice(),
        });
    }
    maps
}

impl RowByteMap {
    /// Return the column index in `columns`/`rope_offsets` for the
    /// character at `rope_offset`, assuming `rope_offset` lies within
    /// `buffer_start_offset..buffer_end_offset`. Out-of-range offsets
    /// clamp to the nearest endpoint.
    pub(crate) fn column_for_rope_offset(&self, rope_offset: usize) -> usize {
        // Find the largest index whose rope offset is <= `rope_offset`.
        let idx = self.rope_offsets.partition_point(|&o| o <= rope_offset);
        idx.saturating_sub(1).min(self.columns.len() - 1)
    }
}

fn visible_byte_range(
    snapshot: &DisplaySnapshot,
    range: &Range<u32>,
    rope_len: usize,
) -> Range<usize> {
    let start_pt = snapshot
        .display_to_buffer(DisplayPoint::new(range.start, 0))
        .unwrap_or(stoat_text::Point::zero());
    let end_pt = snapshot
        .display_to_buffer(DisplayPoint::new(range.end, 0))
        .unwrap_or_else(|| snapshot.buffer_snapshot().rope().max_point());
    let rope = snapshot.buffer_snapshot().rope();
    let start = rope.point_to_offset(start_pt).min(rope_len);
    let end = rope.point_to_offset(end_pt).min(rope_len);
    if end < start {
        return start..start;
    }
    start..end
}

fn push_syntax_runs(
    rows: &mut [RenderedRow],
    byte_maps: &[RowByteMap],
    node_range: Range<usize>,
    style: gpui::HighlightStyle,
) {
    if node_range.is_empty() || byte_maps.is_empty() {
        return;
    }
    let first_idx = byte_maps.partition_point(|m| m.buffer_end_offset <= node_range.start);
    for (row_idx, map) in byte_maps.iter().enumerate().skip(first_idx) {
        if map.buffer_start_offset >= node_range.end {
            break;
        }
        let intersect_start = map.buffer_start_offset.max(node_range.start);
        let intersect_end = map.buffer_end_offset.min(node_range.end);
        if intersect_start >= intersect_end {
            continue;
        }
        let start_col = map.column_for_rope_offset(intersect_start);
        let end_col = map.column_for_rope_offset(intersect_end.saturating_sub(1)) + 1;
        if end_col <= start_col {
            continue;
        }
        let cell_start = map.columns[start_col];
        let cell_end = map.columns[end_col.min(map.columns.len() - 1)];
        if cell_start >= cell_end {
            continue;
        }
        rows[row_idx].runs.push((cell_start..cell_end, style));
    }
}

/// Overlay the move-highlight underline on byte ranges within
/// review rows. Each `(buffer_row, range)` entry corresponds to a
/// [`stoat::review::ReviewSide::moved_spans`] range on that buffer
/// row; the run is pushed in addition to any existing add/delete
/// coloring so the move color (cyan) shows through. Spans that
/// extend past the row's byte length clamp to the row, and spans
/// whose row does not map to a regular display row in `range` are
/// skipped.
pub(crate) fn apply_review_moved_overlay(
    rows: &mut [RenderedRow],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
    moved_spans: &[(u32, Range<usize>)],
) {
    if moved_spans.is_empty() {
        return;
    }
    for (idx, row) in rows.iter_mut().enumerate() {
        let display_row = range.start + idx as u32;
        if !matches!(
            snapshot.classify_row(display_row),
            BlockRowKind::BufferRow { .. }
        ) {
            continue;
        }
        let Some(buffer_point) = snapshot.display_to_buffer(DisplayPoint::new(display_row, 0))
        else {
            continue;
        };
        let buffer_row = buffer_point.row;
        let row_len = row.text.len();
        for (row_idx, span) in moved_spans {
            if *row_idx != buffer_row {
                continue;
            }
            let start = span.start.min(row_len);
            let end = span.end.min(row_len);
            if start >= end {
                continue;
            }
            row.runs
                .push((start..end, token_overlay_style(DIFF_MOVED_HEX)));
        }
    }
}

/// Append a `<- basename:line+1` chip after each buffer row whose
/// diff token detail carries a cross-file move provenance. Mirrors
/// the TUI's `stoat::render::review::render_move_chip` so cross-file
/// moves visible inline against the current buffer carry the same
/// pointer to their source location. Skipped silently when:
///
/// - The row is a block row (deletion blocks, etc.); the chip is buffer-side only.
/// - The buffer row's `token_detail` has no `move_metadata` spans, or only intra-file moves
///   (`source.buffer.is_none()`).
///
/// The chip text is appended after a two-space gap with a run that
/// colors the chip glyphs in the diff move color. The TUI uses a
/// session-resolved `rel_path`; this overlay uses the source path's
/// basename because the workspace rel-path resolver is not in scope
/// here.
pub(crate) fn apply_move_chip_overlay(
    rows: &mut [RenderedRow],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
) {
    let Some(diff_map) = snapshot.diff_map() else {
        return;
    };
    let chip_style = gpui::HighlightStyle {
        color: Some(rgb(DIFF_MOVED_HEX).into()),
        ..Default::default()
    };
    for (idx, row) in rows.iter_mut().enumerate() {
        let display_row = range.start + idx as u32;
        if !matches!(
            snapshot.classify_row(display_row),
            BlockRowKind::BufferRow { .. }
        ) {
            continue;
        }
        let Some(buffer_point) = snapshot.display_to_buffer(DisplayPoint::new(display_row, 0))
        else {
            continue;
        };
        let buffer_row = buffer_point.row;
        let Some(detail) = diff_map.token_detail_for_line(buffer_row) else {
            continue;
        };
        let Some(chip_text) = detail.buffer_spans.iter().find_map(|span| {
            let meta = span.move_metadata.as_ref()?;
            let source = meta.sources.first()?;
            let path = source.buffer.as_ref()?.path.as_path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(format!("  <- {}:{}", name, source.line_range.start + 1))
        }) else {
            continue;
        };
        let mut text = row.text.as_ref().to_string();
        let chip_start = text.len();
        text.push_str(&chip_text);
        let chip_end = text.len();
        row.text = SharedString::from(text);
        row.runs.push((chip_start..chip_end, chip_style));
    }
}

/// Overlay one- or two-character `goto_word` jump labels at the
/// buffer offset each label points at. Each label character
/// replaces the underlying glyph in the rendered row's text and
/// gets a background+foreground run drawn on top of any earlier
/// runs. Characters of the label that already match the user's
/// typed prefix (`input`) paint in `prefix_color`; remaining
/// characters paint in `label_color`. Labels whose target falls
/// outside `range` are skipped.
pub(crate) fn apply_goto_word_overlay(
    rows: &mut [RenderedRow],
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
    labels: &BTreeMap<String, usize>,
    input: &str,
    label_color: Hsla,
    prefix_color: Hsla,
) {
    if labels.is_empty() || rows.is_empty() {
        return;
    }
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let rope_len = rope.len();
    for (label, &offset) in labels {
        if !label.starts_with(input) {
            continue;
        }
        if offset > rope_len {
            continue;
        }
        let point = rope.offset_to_point(offset);
        let display = snapshot.buffer_to_display(point);
        if display.row < range.start || display.row >= range.end {
            continue;
        }
        let row_idx = (display.row - range.start) as usize;
        paint_goto_word_label(
            &mut rows[row_idx],
            display.column,
            label,
            input.len(),
            label_color,
            prefix_color,
        );
    }
}

fn paint_goto_word_label(
    row: &mut RenderedRow,
    start_column: u32,
    label: &str,
    input_len: usize,
    label_color: Hsla,
    prefix_color: Hsla,
) {
    let mut text_owned: String = row.text.as_ref().to_string();
    for (i, ch) in label.chars().enumerate() {
        let column = start_column + i as u32;
        let cell_start = column_to_byte_offset(&text_owned, column);
        let cell_next = column_to_byte_offset(&text_owned, column + 1);
        if cell_start >= text_owned.len() {
            let appended_start = text_owned.len();
            text_owned.push(ch);
            push_label_run(
                &mut row.runs,
                appended_start..text_owned.len(),
                i < input_len,
                label_color,
                prefix_color,
            );
            continue;
        }
        let cell_end = if cell_next > cell_start {
            cell_next
        } else {
            cell_start
                + text_owned[cell_start..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0)
        };
        let new_bytes = ch.encode_utf8(&mut [0u8; 4]).to_string();
        text_owned.replace_range(cell_start..cell_end, &new_bytes);
        let new_end = cell_start + new_bytes.len();
        let delta = new_end as isize - cell_end as isize;
        shift_existing_runs(&mut row.runs, cell_start, delta);
        push_label_run(
            &mut row.runs,
            cell_start..new_end,
            i < input_len,
            label_color,
            prefix_color,
        );
    }
    row.text = SharedString::from(text_owned);
}

fn shift_existing_runs(
    runs: &mut [(Range<usize>, gpui::HighlightStyle)],
    after: usize,
    delta: isize,
) {
    if delta == 0 {
        return;
    }
    for (range, _) in runs.iter_mut() {
        if range.start >= after {
            range.start = (range.start as isize + delta).max(0) as usize;
        }
        if range.end >= after {
            range.end = (range.end as isize + delta).max(0) as usize;
        }
    }
}

fn push_label_run(
    runs: &mut Vec<(Range<usize>, gpui::HighlightStyle)>,
    range: Range<usize>,
    is_prefix: bool,
    label_color: Hsla,
    prefix_color: Hsla,
) {
    let style = gpui::HighlightStyle {
        background_color: Some(if is_prefix { prefix_color } else { label_color }),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    runs.push((range, style));
}

fn append_run(
    text: &mut String,
    runs: &mut Vec<(Range<usize>, gpui::HighlightStyle)>,
    segment: &str,
    style: Option<gpui::HighlightStyle>,
) {
    if segment.is_empty() {
        return;
    }
    let start = text.len();
    text.push_str(segment);
    let end = text.len();
    let Some(style) = style else {
        return;
    };
    if let Some((last_range, last_style)) = runs.last_mut() {
        if *last_style == style && last_range.end == start {
            last_range.end = end;
            return;
        }
    }
    runs.push((start..end, style));
}

pub(crate) fn render_row_element(row: RenderedRow) -> Div {
    let RenderedRow { text, runs } = row;
    div().child(StyledText::new(text).with_highlights(runs))
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct SelectionPaint {
    pub row_selection_spans: BTreeMap<u32, Vec<Range<usize>>>,
    pub row_cursors: BTreeMap<u32, Vec<usize>>,
    pub active_line_row: Option<u32>,
}

pub(crate) fn compute_selection_paint(
    snapshot: &DisplaySnapshot,
    selections: &[Selection<Anchor>],
    rendered_rows: &[RenderedRow],
    start_row: u32,
) -> SelectionPaint {
    let mut paint = SelectionPaint::default();
    let end_row = start_row + rendered_rows.len() as u32;
    let buffer = snapshot.buffer_snapshot();

    let primary_id = selections.iter().map(|s| s.id).max();
    if let Some(id) = primary_id {
        let primary = selections
            .iter()
            .find(|s| s.id == id)
            .expect("primary id must come from the selections slice");
        let head_offset = buffer.resolve_anchor(&primary.head());
        let head_point = buffer.rope().offset_to_point(head_offset);
        let head_display = snapshot.buffer_to_display(head_point);
        if head_display.row >= start_row && head_display.row < end_row {
            paint.active_line_row = Some(head_display.row);
        }
    }

    for selection in selections {
        let start_offset = buffer.resolve_anchor(&selection.start);
        let end_offset = buffer.resolve_anchor(&selection.end);
        let (lo, hi) = if start_offset <= end_offset {
            (start_offset, end_offset)
        } else {
            (end_offset, start_offset)
        };
        let head_offset = buffer.resolve_anchor(&selection.head());

        if lo != hi {
            let lo_point = buffer.rope().offset_to_point(lo);
            let hi_point = buffer.rope().offset_to_point(hi);
            let lo_display = snapshot.buffer_to_display(lo_point);
            let hi_display = snapshot.buffer_to_display(hi_point);
            for row in lo_display.row..=hi_display.row {
                if row < start_row || row >= end_row {
                    continue;
                }
                let row_idx = (row - start_row) as usize;
                let row_text: &str = rendered_rows[row_idx].text.as_ref();
                let row_char_count = row_text.chars().count() as u32;
                let start_col = if row == lo_display.row {
                    lo_display.column
                } else {
                    0
                };
                let end_col = if row == hi_display.row {
                    hi_display.column
                } else {
                    row_char_count
                };
                if start_col == end_col {
                    continue;
                }
                let start_byte = column_to_byte_offset(row_text, start_col);
                let end_byte = column_to_byte_offset(row_text, end_col);
                paint
                    .row_selection_spans
                    .entry(row)
                    .or_default()
                    .push(start_byte..end_byte);
            }
        }

        let head_point = buffer.rope().offset_to_point(head_offset);
        let head_display = snapshot.buffer_to_display(head_point);
        if head_display.row >= start_row && head_display.row < end_row {
            let row_idx = (head_display.row - start_row) as usize;
            let row_text: &str = rendered_rows[row_idx].text.as_ref();
            let byte = column_to_byte_offset(row_text, head_display.column);
            paint
                .row_cursors
                .entry(head_display.row)
                .or_default()
                .push(byte);
        }
    }

    paint
}

fn column_to_byte_offset(row_text: &str, column: u32) -> usize {
    row_text
        .char_indices()
        .nth(column as usize)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(row_text.len())
}

pub(crate) fn apply_selection_paint(
    row: RenderedRow,
    display_row: u32,
    paint: &SelectionPaint,
    selection_color: Hsla,
    cursor_color: Hsla,
    cursor_text_color: Hsla,
    active_line_color: Hsla,
) -> RenderedRow {
    let RenderedRow {
        text,
        runs: syntax_runs,
    } = row;

    let active_line = paint.active_line_row == Some(display_row);
    let empty: Vec<Range<usize>> = Vec::new();
    let selection_spans = paint
        .row_selection_spans
        .get(&display_row)
        .unwrap_or(&empty);
    let cursor_offsets: &[usize] = paint
        .row_cursors
        .get(&display_row)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // Fast path: nothing overlays this row, return the input unchanged.
    // `SharedString` is moved through without cloning the underlying
    // `Arc<str>` and gpui's `compute_runs` fills any gaps in
    // `syntax_runs` with the default text style at paint time.
    if !active_line && selection_spans.is_empty() && cursor_offsets.is_empty() {
        return RenderedRow {
            text,
            runs: syntax_runs,
        };
    }

    let text_len_borrowed = text.as_ref().len();
    let needs_pad = (active_line && text_len_borrowed == 0)
        || cursor_offsets.iter().any(|&o| o >= text_len_borrowed);

    let active_line_style = active_line.then(|| gpui::HighlightStyle {
        background_color: Some(active_line_color),
        ..Default::default()
    });
    let selection_style = gpui::HighlightStyle {
        background_color: Some(selection_color),
        ..Default::default()
    };
    let cursor_style = gpui::HighlightStyle {
        background_color: Some(cursor_color),
        color: Some(cursor_text_color),
        ..Default::default()
    };

    if !needs_pad {
        // Slow path without allocation: build the merged sweep against
        // the borrowed text and return the original `SharedString`.
        let mut cursor_ranges: Vec<Range<usize>> = Vec::with_capacity(cursor_offsets.len());
        for &offset in cursor_offsets {
            let after = text.as_ref()[offset..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            cursor_ranges.push(offset..offset + after);
        }
        let merged = merge_selection_runs(
            text_len_borrowed,
            &syntax_runs,
            active_line_style,
            selection_spans,
            selection_style,
            &cursor_ranges,
            cursor_style,
        );
        return RenderedRow { text, runs: merged };
    }

    // Padding required: materialize a fresh `String` so we can append
    // a space for the active-line-on-empty-row case or for end-of-line
    // cursors.
    let mut text_owned: String = text.as_ref().to_string();
    if active_line && text_owned.is_empty() {
        text_owned.push(' ');
    }
    let mut cursor_ranges: Vec<Range<usize>> = Vec::with_capacity(cursor_offsets.len());
    for &offset in cursor_offsets {
        if offset < text_owned.len() {
            let after = text_owned[offset..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            cursor_ranges.push(offset..offset + after);
        } else {
            let appended_start = text_owned.len();
            text_owned.push(' ');
            cursor_ranges.push(appended_start..text_owned.len());
        }
    }
    let text_len = text_owned.len();
    let merged = merge_selection_runs(
        text_len,
        &syntax_runs,
        active_line_style,
        selection_spans,
        selection_style,
        &cursor_ranges,
        cursor_style,
    );
    RenderedRow {
        text: SharedString::from(text_owned),
        runs: merged,
    }
}

fn merge_selection_runs(
    text_len: usize,
    syntax_runs: &[(Range<usize>, gpui::HighlightStyle)],
    active_line_style: Option<gpui::HighlightStyle>,
    selection_spans: &[Range<usize>],
    selection_style: gpui::HighlightStyle,
    cursor_ranges: &[Range<usize>],
    cursor_style: gpui::HighlightStyle,
) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
    if text_len == 0 {
        return syntax_runs.to_vec();
    }
    let mut breakpoints: BTreeSet<usize> = BTreeSet::new();
    breakpoints.insert(0);
    breakpoints.insert(text_len);
    for (r, _) in syntax_runs {
        breakpoints.insert(r.start.min(text_len));
        breakpoints.insert(r.end.min(text_len));
    }
    for r in selection_spans {
        breakpoints.insert(r.start.min(text_len));
        breakpoints.insert(r.end.min(text_len));
    }
    for r in cursor_ranges {
        breakpoints.insert(r.start);
        breakpoints.insert(r.end);
    }
    let breakpoints: Vec<usize> = breakpoints.into_iter().collect();

    let mut merged: Vec<(Range<usize>, gpui::HighlightStyle)> = Vec::new();
    for window in breakpoints.windows(2) {
        let (a, b) = (window[0], window[1]);
        if a >= b {
            continue;
        }
        let mut style = gpui::HighlightStyle::default();
        if let Some(&(_, syntax)) = syntax_runs.iter().find(|(r, _)| r.start <= a && r.end >= b) {
            style = style.highlight(syntax);
        }
        if let Some(al) = active_line_style {
            style = style.highlight(al);
        }
        if selection_spans.iter().any(|r| r.start <= a && r.end >= b) {
            style = style.highlight(selection_style);
        }
        if cursor_ranges.iter().any(|r| r.start <= a && r.end >= b) {
            style = style.highlight(cursor_style);
        }
        if let Some((last_range, last_style)) = merged.last_mut() {
            if *last_style == style && last_range.end == a {
                last_range.end = b;
                continue;
            }
        }
        merged.push((a..b, style));
    }
    merged
}

/// Convert a fractional `scroll_position.y` (in display rows) to the
/// pixel offset suitable for `ScrollHandle::set_offset`. Returns a
/// negative pixel value because gpui scrolls content up by holding a
/// negative offset on the inner list. With this offset, `uniform_list`
/// paints each visible row at
/// `padded_bounds.origin.y + visible_ix * line_height - (scroll_position.y * line_height) %
/// line_height`, matching the sub-pixel formula in
/// `references/zed/crates/editor/src/element.rs:3575-3590`.
pub(crate) fn scroll_position_to_pixel_offset_y(
    scroll_position_y: f64,
    line_height: Pixels,
) -> Pixels {
    Pixels::from(-(scroll_position_y * f64::from(f32::from(line_height))) as f32)
}

const DIFF_ADDED_HEX: u32 = 0x4caf50;
const DIFF_MODIFIED_HEX: u32 = 0x2196f3;
const DIFF_MOVED_HEX: u32 = 0x00bcd4;
const DIFF_DELETED_HEX: u32 = 0xf44336;
const DIFF_STAGED_ADDED_HEX: u32 = 0xbbb529;
const DIFF_STAGED_MODIFIED_HEX: u32 = 0xd4aa32;
const DIFF_STAGED_DELETED_HEX: u32 = 0xd08840;
const DIFF_COMMITTED_ADDED_HEX: u32 = 0x9b7ed8;
const DIFF_COMMITTED_MODIFIED_HEX: u32 = 0x8470c4;
const DIFF_COMMITTED_DELETED_HEX: u32 = 0xb07cc0;
const DIAG_ERROR_HEX: u32 = 0xe53935;
const DIAG_WARNING_HEX: u32 = 0xffb300;
const DIAG_INFO_HEX: u32 = 0x29b6f6;
const DIAG_HINT_HEX: u32 = 0x9e9e9e;
const BLOCK_TEXT_HEX: u32 = 0xa0a0a0;

/// Visible characters in the blame strip when active:
/// `{short_sha:7} {first_name:<8} {age:>3} ` separator → 21 cells.
pub(crate) const BLAME_STRIP_WIDTH: usize = 21;
const BLAME_SHA_WIDTH: usize = 7;
const BLAME_NAME_WIDTH: usize = 8;
const BLAME_AGE_WIDTH: usize = 3;
const BLAME_SHA_HEX: u32 = 0xc9b458;
const BLAME_NAME_HEX: u32 = 0x73c991;
const BLAME_AGE_HEX: u32 = 0x6796e6;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GutterMetrics {
    pub line_number_width: usize,
    pub blame_width: usize,
    pub total_width: usize,
}

pub(crate) fn gutter_metrics(snapshot: &DisplaySnapshot, blame_visible: bool) -> GutterMetrics {
    let buffer_line_count = snapshot.buffer_line_count().max(1);
    let line_number_width = digit_count(buffer_line_count);
    let blame_width = if blame_visible { BLAME_STRIP_WIDTH } else { 0 };
    GutterMetrics {
        line_number_width,
        blame_width,
        total_width: blame_width + line_number_width + 1 + 1 + 1 + 1,
    }
}

fn chunk_glyph_for(status: ChunkStatus) -> (char, u32) {
    match status {
        ChunkStatus::Pending => ('>', DIAG_HINT_HEX),
        ChunkStatus::Staged => ('+', DIFF_ADDED_HEX),
        ChunkStatus::Unstaged => ('o', DIFF_DELETED_HEX),
        ChunkStatus::Skipped => ('x', DIAG_HINT_HEX),
    }
}

fn digit_count(mut n: u32) -> usize {
    let mut digits = 1usize;
    while n >= 10 {
        digits += 1;
        n /= 10;
    }
    digits
}

pub(crate) type DiagnosticRowMap = BTreeMap<u32, DiagnosticSeverity>;

pub(crate) fn compute_row_severity_for_path(
    diagnostics: &DiagnosticSet,
    path: &Path,
) -> DiagnosticRowMap {
    let mut out: DiagnosticRowMap = BTreeMap::new();
    for diag in diagnostics.get(path) {
        let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
        let start_line = diag.range.start.line;
        let end_line = diag.range.end.line;
        for row in start_line..=end_line {
            out.entry(row)
                .and_modify(|cur| {
                    if severity_rank(sev) < severity_rank(*cur) {
                        *cur = sev;
                    }
                })
                .or_insert(sev);
        }
    }
    out
}

fn severity_rank(sev: DiagnosticSeverity) -> u8 {
    match sev {
        DiagnosticSeverity::ERROR => 0,
        DiagnosticSeverity::WARNING => 1,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 3,
        _ => 0,
    }
}

fn diff_strip_for_status(status: stoat::DiffStatus) -> Option<(char, u32)> {
    match status {
        stoat::DiffStatus::Unchanged => None,
        stoat::DiffStatus::Added => Some(('|', DIFF_ADDED_HEX)),
        stoat::DiffStatus::Modified => Some(('|', DIFF_MODIFIED_HEX)),
        stoat::DiffStatus::Moved => Some(('|', DIFF_MOVED_HEX)),
        stoat::DiffStatus::StagedAdded => Some(('|', DIFF_STAGED_ADDED_HEX)),
        stoat::DiffStatus::StagedModified => Some(('|', DIFF_STAGED_MODIFIED_HEX)),
        stoat::DiffStatus::StagedDeleted => Some(('|', DIFF_STAGED_DELETED_HEX)),
        stoat::DiffStatus::CommittedAdded => Some(('|', DIFF_COMMITTED_ADDED_HEX)),
        stoat::DiffStatus::CommittedModified => Some(('|', DIFF_COMMITTED_MODIFIED_HEX)),
        stoat::DiffStatus::CommittedDeleted => Some(('|', DIFF_COMMITTED_DELETED_HEX)),
    }
}

fn diagnostic_glyph_for(severity: DiagnosticSeverity) -> (char, u32) {
    match severity {
        DiagnosticSeverity::ERROR => ('E', DIAG_ERROR_HEX),
        DiagnosticSeverity::WARNING => ('W', DIAG_WARNING_HEX),
        DiagnosticSeverity::INFORMATION => ('I', DIAG_INFO_HEX),
        DiagnosticSeverity::HINT => ('H', DIAG_HINT_HEX),
        _ => ('E', DIAG_ERROR_HEX),
    }
}

pub(crate) struct GutterPaint<'a> {
    pub display_snapshot: &'a DisplaySnapshot,
    pub diff_map: &'a stoat::DiffMap,
    pub diagnostics: Option<&'a DiagnosticRowMap>,
    pub review_chunk_markers: &'a [(u32, ChunkStatus)],
    pub review_move_provenances: &'a [(u32, MoveProvenance)],
    pub blame: Option<BlamePaint<'a>>,
    pub metrics: GutterMetrics,
    pub line_number_color: Hsla,
}

/// Per-row blame entries plus the `now` reference used to format
/// relative ages. Carried on [`GutterPaint`] when the editor has its
/// blame strip toggled visible and the per-buffer [`BlameState`]
/// holds populated entries.
pub(crate) struct BlamePaint<'a> {
    pub lines: &'a [BlameLine],
    pub now_seconds: i64,
}

pub(crate) struct RowSuffix {
    text: String,
    runs: Vec<(Range<usize>, gpui::HighlightStyle)>,
}

fn build_row_suffix(buffer_row: Option<u32>, paint: &GutterPaint<'_>) -> RowSuffix {
    let mut text = String::new();
    let mut runs: Vec<(Range<usize>, gpui::HighlightStyle)> = Vec::new();
    let Some(buffer_row) = buffer_row else {
        return RowSuffix { text, runs };
    };
    let Some(provenance) = paint
        .review_move_provenances
        .iter()
        .find(|(row, _)| *row == buffer_row)
        .map(|(_, prov)| prov)
    else {
        return RowSuffix { text, runs };
    };

    let chip = format!("  <- {}:{}", provenance.rel_path, provenance.line + 1);
    let start = text.len();
    text.push_str(&chip);
    runs.push((
        start..text.len(),
        gpui::HighlightStyle {
            color: Some(rgb(DIFF_MOVED_HEX).into()),
            ..Default::default()
        },
    ));
    RowSuffix { text, runs }
}

pub(crate) fn render_row_with_gutter(
    row: RenderedRow,
    display_row: u32,
    paint: &GutterPaint<'_>,
) -> Div {
    let (prefix, body, suffix) = build_gutter_row_pieces(row, display_row, paint);
    div()
        .flex()
        .flex_row()
        .child(StyledText::new(SharedString::from(prefix.text)).with_highlights(prefix.runs))
        .child(StyledText::new(body.text).with_highlights(body.runs))
        .child(StyledText::new(SharedString::from(suffix.text)).with_highlights(suffix.runs))
}

/// Build the three render pieces of a gutter row -- prefix (line
/// number, diagnostic glyph, diff marker, blame strip), the body
/// `RenderedRow` passed through unchanged, and the suffix (review
/// move chip). Splitting this out of [`render_row_with_gutter`]
/// lets tests observe the body byte buffer to confirm the row's
/// `SharedString` and runs move through without reallocation.
pub(crate) fn build_gutter_row_pieces(
    row: RenderedRow,
    display_row: u32,
    paint: &GutterPaint<'_>,
) -> (GutterPrefix, RenderedRow, RowSuffix) {
    let prefix = build_gutter_prefix(display_row, paint);
    let buffer_row = paint
        .display_snapshot
        .display_to_buffer(DisplayPoint::new(display_row, 0))
        .map(|p| p.row);
    let suffix = build_row_suffix(buffer_row, paint);
    (prefix, row, suffix)
}

pub(crate) struct GutterPrefix {
    text: String,
    runs: Vec<(Range<usize>, gpui::HighlightStyle)>,
}

fn build_gutter_prefix(display_row: u32, paint: &GutterPaint<'_>) -> GutterPrefix {
    let mut text = String::new();
    let mut runs: Vec<(Range<usize>, gpui::HighlightStyle)> = Vec::new();
    let width = paint.metrics.line_number_width;

    let buffer_row = paint
        .display_snapshot
        .display_to_buffer(DisplayPoint::new(display_row, 0))
        .map(|p| p.row);
    let show_line_number =
        buffer_row.is_some() && !paint.display_snapshot.is_wrap_continuation(display_row);

    if let Some(blame) = paint.blame.as_ref() {
        append_blame_strip(&mut text, &mut runs, buffer_row, show_line_number, blame);
    }

    if let (Some(row), true) = (buffer_row, show_line_number) {
        let line_str = format!("{:>width$}", row + 1, width = width);
        let start = text.len();
        text.push_str(&line_str);
        let end = text.len();
        let style = gpui::HighlightStyle {
            color: Some(paint.line_number_color),
            ..Default::default()
        };
        runs.push((start..end, style));
    } else {
        for _ in 0..width {
            text.push(' ');
        }
    }

    let diff_status = buffer_row
        .map(|row| paint.diff_map.status_for_line(row))
        .unwrap_or(stoat::DiffStatus::Unchanged);
    let deletion_above = buffer_row
        .and_then(|row| row.checked_sub(1))
        .map(|prev| paint.diff_map.has_deletion_after(prev))
        .unwrap_or(false);
    let start = text.len();
    if let Some((ch, hex)) = diff_strip_for_status(diff_status) {
        text.push(ch);
        let end = text.len();
        let style = gpui::HighlightStyle {
            color: Some(rgb(hex).into()),
            ..Default::default()
        };
        runs.push((start..end, style));
    } else if deletion_above {
        text.push('^');
        let end = text.len();
        let style = gpui::HighlightStyle {
            color: Some(rgb(DIFF_DELETED_HEX).into()),
            ..Default::default()
        };
        runs.push((start..end, style));
    } else {
        text.push(' ');
    }

    let diag_severity =
        buffer_row.and_then(|row| paint.diagnostics.and_then(|map| map.get(&row).copied()));
    let start = text.len();
    if let Some(sev) = diag_severity {
        let (ch, hex) = diagnostic_glyph_for(sev);
        text.push(ch);
        let end = text.len();
        let style = gpui::HighlightStyle {
            color: Some(rgb(hex).into()),
            ..Default::default()
        };
        runs.push((start..end, style));
    } else {
        text.push(' ');
    }

    let chunk_status = buffer_row.and_then(|row| {
        paint
            .review_chunk_markers
            .iter()
            .find(|(start, _)| *start == row)
            .map(|(_, status)| *status)
    });
    let start = text.len();
    if let Some(status) = chunk_status {
        let (ch, hex) = chunk_glyph_for(status);
        text.push(ch);
        let end = text.len();
        let style = gpui::HighlightStyle {
            color: Some(rgb(hex).into()),
            ..Default::default()
        };
        runs.push((start..end, style));
    } else {
        text.push(' ');
    }

    text.push(' ');

    GutterPrefix { text, runs }
}

/// Prepend the blame strip onto `text` / `runs`. When the row is a
/// wrap continuation, has no buffer mapping, or no blame entry covers
/// `buffer_row`, the strip is rendered as [`BLAME_STRIP_WIDTH`] blank
/// cells so column alignment with neighbour rows is preserved.
fn append_blame_strip(
    text: &mut String,
    runs: &mut Vec<(Range<usize>, gpui::HighlightStyle)>,
    buffer_row: Option<u32>,
    show_for_row: bool,
    paint: &BlamePaint<'_>,
) {
    let entry = if show_for_row {
        buffer_row.and_then(|row| paint.lines.iter().find(|line| line.line == row))
    } else {
        None
    };
    let Some(entry) = entry else {
        for _ in 0..BLAME_STRIP_WIDTH {
            text.push(' ');
        }
        return;
    };

    let start = text.len();
    push_padded_chars(text, &entry.short_sha, BLAME_SHA_WIDTH, false);
    runs.push((start..text.len(), color_style(BLAME_SHA_HEX)));
    text.push(' ');

    let first_name = blame::author_first_name(&entry.author_name, BLAME_NAME_WIDTH);
    let start = text.len();
    push_padded_chars(text, &first_name, BLAME_NAME_WIDTH, false);
    runs.push((start..text.len(), color_style(BLAME_NAME_HEX)));
    text.push(' ');

    let age = blame::format_age_short(entry.time, paint.now_seconds);
    let start = text.len();
    push_padded_chars(text, &age, BLAME_AGE_WIDTH, true);
    runs.push((start..text.len(), color_style(BLAME_AGE_HEX)));
    text.push(' ');
}

fn push_padded_chars(text: &mut String, value: &str, width: usize, right_align: bool) {
    let chars: Vec<char> = value.chars().take(width).collect();
    let pad = width.saturating_sub(chars.len());
    if right_align {
        for _ in 0..pad {
            text.push(' ');
        }
        for ch in chars {
            text.push(ch);
        }
    } else {
        for ch in chars {
            text.push(ch);
        }
        for _ in 0..pad {
            text.push(' ');
        }
    }
}

fn color_style(hex: u32) -> gpui::HighlightStyle {
    gpui::HighlightStyle {
        color: Some(rgb(hex).into()),
        ..Default::default()
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::{buffer::Buffer, display_map::DisplayMap};
    use gpui::{hsla, AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn hex_of(color: Hsla) -> u32 {
        let rgba: gpui::Rgba = color.into();
        let r = (rgba.r * 255.0).round() as u32;
        let g = (rgba.g * 255.0).round() as u32;
        let b = (rgba.b * 255.0).round() as u32;
        (r << 16) | (g << 8) | b
    }

    #[test]
    fn scroll_position_to_pixel_offset_y_zero() {
        let offset = scroll_position_to_pixel_offset_y(0.0, px(16.0));
        assert_eq!(f32::from(offset), 0.0);
    }

    #[test]
    fn scroll_position_to_pixel_offset_y_integer_row() {
        let offset = scroll_position_to_pixel_offset_y(3.0, px(16.0));
        assert_eq!(f32::from(offset), -48.0);
    }

    #[test]
    fn scroll_position_to_pixel_offset_y_fractional_row() {
        let offset = scroll_position_to_pixel_offset_y(2.5, px(16.0));
        assert_eq!(f32::from(offset), -40.0);
    }

    #[test]
    fn scroll_position_to_pixel_offset_y_sub_pixel_fraction() {
        let offset = scroll_position_to_pixel_offset_y(0.25, px(16.0));
        assert_eq!(f32::from(offset), -4.0);
    }

    #[test]
    fn ratatui_color_to_hsla_named_colors() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Black).map(hex_of),
            Some(0x000000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Red).map(hex_of),
            Some(0xcd0000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::White).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_rgb_passthrough() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Rgb(0x12, 0x34, 0x56)).map(hex_of),
            Some(0x123456),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_named() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(1)).map(hex_of),
            Some(0xcd0000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(15)).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_cube() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(16)).map(hex_of),
            Some(0x000000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(231)).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_grayscale() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(232)).map(hex_of),
            Some(0x080808),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(255)).map(hex_of),
            Some(0xeeeeee),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_reset_returns_none() {
        assert_eq!(ratatui_color_to_hsla(Color::Reset), None);
    }

    fn stoat_style(
        foreground: Option<Color>,
        background: Option<Color>,
        bold: Option<bool>,
        italic: Option<bool>,
        underline: Option<bool>,
        strikethrough: Option<bool>,
    ) -> StoatHighlightStyle {
        StoatHighlightStyle {
            foreground,
            background,
            bold,
            italic,
            underline,
            strikethrough,
        }
    }

    #[test]
    fn convert_highlight_style_passes_through_colors() {
        let style = stoat_style(Some(Color::Red), Some(Color::Blue), None, None, None, None);
        let converted = convert_highlight_style(&style);
        assert_eq!(converted.color.map(hex_of), Some(0xcd0000));
        assert_eq!(converted.background_color.map(hex_of), Some(0x0000ee));
        assert_eq!(converted.font_weight, None);
        assert_eq!(converted.font_style, None);
    }

    #[test]
    fn convert_highlight_style_maps_bold_to_font_weight() {
        let style = stoat_style(None, None, Some(true), None, None, None);
        assert_eq!(
            convert_highlight_style(&style).font_weight,
            Some(FontWeight::BOLD),
        );

        let unset = stoat_style(None, None, Some(false), None, None, None);
        assert_eq!(convert_highlight_style(&unset).font_weight, None);
    }

    #[test]
    fn convert_highlight_style_maps_italic_to_font_style() {
        let style = stoat_style(None, None, None, Some(true), None, None);
        assert_eq!(
            convert_highlight_style(&style).font_style,
            Some(FontStyle::Italic),
        );

        let unset = stoat_style(None, None, None, Some(false), None, None);
        assert_eq!(convert_highlight_style(&unset).font_style, None);
    }

    #[test]
    fn convert_highlight_style_maps_underline_and_strikethrough() {
        let style = stoat_style(None, None, None, None, Some(true), Some(true));
        let converted = convert_highlight_style(&style);
        assert_eq!(
            converted.underline,
            Some(UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            }),
        );
        assert_eq!(
            converted.strikethrough,
            Some(StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
        );
    }

    fn test_snapshot(cx: &mut TestAppContext, text: &str) -> DisplaySnapshot {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        display_map.update(cx, |dm, _| dm.snapshot())
    }

    #[test]
    fn build_rendered_rows_single_line() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello");

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.as_ref(), "hello");
        assert!(rows[0].runs.is_empty());
    }

    #[test]
    fn build_rendered_rows_splits_on_newline() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "ab\ncd\nef");

        let rows = build_rendered_rows(&snapshot, 0..3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text.as_ref(), "ab");
        assert_eq!(rows[1].text.as_ref(), "cd");
        assert_eq!(rows[2].text.as_ref(), "ef");
    }

    #[test]
    fn build_rendered_rows_groups_styled_runs() {
        let mut runs = Vec::<(Range<usize>, gpui::HighlightStyle)>::new();
        let mut text = String::new();
        let style =
            convert_highlight_style(&stoat_style(Some(Color::Red), None, None, None, None, None));

        append_run(&mut text, &mut runs, "foo", Some(style));
        append_run(&mut text, &mut runs, "bar", Some(style));

        assert_eq!(text, "foobar");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0..6);
    }

    #[test]
    fn gutter_metrics_for_single_line_returns_width_one() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "x");

        let metrics = gutter_metrics(&snapshot, false);
        assert_eq!(metrics.line_number_width, 1);
        assert_eq!(metrics.total_width, 5);
    }

    #[test]
    fn gutter_metrics_for_hundred_lines_uses_digit_count() {
        let mut cx = TestAppContext::single();
        let text = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let snapshot = test_snapshot(&mut cx, &text);

        let metrics = gutter_metrics(&snapshot, false);
        assert_eq!(metrics.line_number_width, 3);
        assert_eq!(metrics.total_width, 7);
    }

    fn diagnostic_set_with(
        cx: &mut TestAppContext,
        path: &Path,
        diags: Vec<lsp_types::Diagnostic>,
    ) -> gpui::Entity<DiagnosticSet> {
        let path = path.to_path_buf();
        let set = cx.update(|cx| cx.new(|_| DiagnosticSet::new()));
        set.update(cx, |s, cx| s.replace_for_path(path, diags, cx));
        set
    }

    fn diag(severity: DiagnosticSeverity, start_line: u32, end_line: u32) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(start_line, 0),
                lsp_types::Position::new(end_line, 0),
            ),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: String::new(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn compute_row_severity_picks_worst_per_row() {
        let mut cx = TestAppContext::single();
        let path = std::path::PathBuf::from("/ws/a.rs");
        let set = diagnostic_set_with(
            &mut cx,
            &path,
            vec![
                diag(DiagnosticSeverity::WARNING, 0, 0),
                diag(DiagnosticSeverity::ERROR, 0, 0),
                diag(DiagnosticSeverity::HINT, 2, 2),
            ],
        );

        let map = set.read_with(&cx, |s, _| compute_row_severity_for_path(s, &path));
        assert_eq!(map.get(&0), Some(&DiagnosticSeverity::ERROR));
        assert_eq!(map.get(&1), None);
        assert_eq!(map.get(&2), Some(&DiagnosticSeverity::HINT));
    }

    #[test]
    fn build_gutter_row_pieces_moves_body_text_through_without_reallocating() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello\nworld");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };
        let original_text = SharedString::from("hello".to_string());
        let original_ptr = original_text.as_ref().as_ptr();
        let runs = vec![(
            0..5,
            gpui::HighlightStyle {
                color: Some(rgb(0x00ff00).into()),
                ..Default::default()
            },
        )];
        let row = RenderedRow {
            text: original_text,
            runs: runs.clone(),
        };

        let (_prefix, body, _suffix) = build_gutter_row_pieces(row, 0, &paint);

        assert_eq!(
            body.text.as_ref().as_ptr(),
            original_ptr,
            "body SharedString must move through without reallocating",
        );
        assert_eq!(body.runs, runs);
    }

    #[test]
    fn render_row_with_gutter_paints_line_number() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello\nworld");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };

        let element = render_row_with_gutter(row, 0, &paint);
        let _ = element;
    }

    #[test]
    fn render_row_with_gutter_blanks_line_number_on_unknown_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };
        let prefix = build_gutter_prefix(0, &paint);
        assert!(prefix.text.starts_with('1'));
    }

    #[test]
    fn build_gutter_prefix_omits_diagnostic_glyph_when_absent() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };
        let prefix = build_gutter_prefix(0, &paint);

        let line_num_width = metrics.line_number_width;
        let diag_position = line_num_width + 1;
        let diag_char = prefix
            .text
            .chars()
            .nth(diag_position)
            .expect("gutter prefix populated");
        assert_eq!(diag_char, ' ');
    }

    #[test]
    fn build_gutter_prefix_paints_diagnostic_glyph_when_severity_present() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let mut diagnostics: DiagnosticRowMap = BTreeMap::new();
        diagnostics.insert(0, DiagnosticSeverity::WARNING);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: Some(&diagnostics),
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };
        let prefix = build_gutter_prefix(0, &paint);

        let diag_position = metrics.line_number_width + 1;
        let diag_char = prefix
            .text
            .chars()
            .nth(diag_position)
            .expect("gutter prefix populated");
        assert_eq!(diag_char, 'W');
    }

    fn diff_strip_char(prefix: &GutterPrefix, line_number_width: usize) -> char {
        prefix
            .text
            .chars()
            .nth(line_number_width)
            .expect("gutter prefix populated")
    }

    fn chunk_glyph_char(prefix: &GutterPrefix, line_number_width: usize) -> char {
        prefix
            .text
            .chars()
            .nth(line_number_width + 2)
            .expect("gutter prefix populated")
    }

    #[test]
    fn build_gutter_prefix_paints_chunk_glyph_when_chunk_starts_at_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let markers = vec![(1, ChunkStatus::Staged)];
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &markers,
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(1, &paint);
        assert_eq!(chunk_glyph_char(&prefix, metrics.line_number_width), '+');
    }

    #[test]
    fn build_gutter_prefix_omits_chunk_glyph_when_no_marker_at_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let markers = vec![(1, ChunkStatus::Staged)];
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &markers,
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(0, &paint);
        assert_eq!(chunk_glyph_char(&prefix, metrics.line_number_width), ' ');
        let prefix = build_gutter_prefix(2, &paint);
        assert_eq!(chunk_glyph_char(&prefix, metrics.line_number_width), ' ');
    }

    #[test]
    fn chunk_glyph_for_maps_each_status() {
        assert_eq!(chunk_glyph_for(ChunkStatus::Pending), ('>', DIAG_HINT_HEX));
        assert_eq!(chunk_glyph_for(ChunkStatus::Staged), ('+', DIFF_ADDED_HEX));
        assert_eq!(
            chunk_glyph_for(ChunkStatus::Unstaged),
            ('o', DIFF_DELETED_HEX)
        );
        assert_eq!(chunk_glyph_for(ChunkStatus::Skipped), ('x', DIAG_HINT_HEX));
    }

    fn sample_blame_line(line: u32, sha: &str, author: &str, time: i64) -> BlameLine {
        BlameLine {
            line,
            commit_sha: format!("{sha}deadbeef"),
            short_sha: sha.to_string(),
            author_name: author.to_string(),
            time,
        }
    }

    #[test]
    fn gutter_metrics_adds_blame_strip_width_when_visible() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "x");

        let off = gutter_metrics(&snapshot, false);
        let on = gutter_metrics(&snapshot, true);
        assert_eq!(off.blame_width, 0);
        assert_eq!(on.blame_width, BLAME_STRIP_WIDTH);
        assert_eq!(on.total_width, off.total_width + BLAME_STRIP_WIDTH);
    }

    #[test]
    fn build_gutter_prefix_paints_blame_columns_when_entry_present() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, true);
        let now = 1_000_000_000i64;
        let lines = vec![sample_blame_line(
            0,
            "abc1234",
            "Ada Lovelace",
            now - 5 * 24 * 60 * 60,
        )];
        let blame = BlamePaint {
            lines: &lines,
            now_seconds: now,
        };
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: Some(blame),
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(0, &paint);
        let strip: String = prefix.text.chars().take(BLAME_STRIP_WIDTH).collect();
        assert_eq!(strip, "abc1234 Ada       5d ");
    }

    #[test]
    fn build_gutter_prefix_blanks_blame_columns_when_no_entry_for_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, true);
        let lines = vec![sample_blame_line(0, "abc1234", "Ada", 0)];
        let blame = BlamePaint {
            lines: &lines,
            now_seconds: 1_000_000_000,
        };
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: Some(blame),
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(1, &paint);
        let strip: String = prefix.text.chars().take(BLAME_STRIP_WIDTH).collect();
        assert_eq!(strip, " ".repeat(BLAME_STRIP_WIDTH));
    }

    #[test]
    fn build_gutter_prefix_truncates_long_first_names_in_blame_strip() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "x");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, true);
        let now = 0i64;
        let lines = vec![sample_blame_line(0, "abcdef0", "Octocatherine Smith", now)];
        let blame = BlamePaint {
            lines: &lines,
            now_seconds: now,
        };
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: Some(blame),
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(0, &paint);
        let strip: String = prefix.text.chars().take(BLAME_STRIP_WIDTH).collect();
        assert_eq!(strip, "abcdef0 Octocath now ");
    }

    #[test]
    fn build_row_suffix_appends_chip_when_provenance_at_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let provenances = vec![(
            1u32,
            MoveProvenance {
                rel_path: "src/foo.rs".to_string(),
                line: 41,
            },
        )];
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &provenances,
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let suffix = build_row_suffix(Some(1), &paint);
        assert_eq!(suffix.text, "  <- src/foo.rs:42");
        assert_eq!(suffix.runs.len(), 1);
        assert_eq!(
            suffix.runs[0].1.color,
            Some(rgb(DIFF_MOVED_HEX).into()),
            "chip text should be painted in the move color",
        );
    }

    #[test]
    fn build_row_suffix_is_empty_when_no_provenance_at_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let provenances = vec![(
            5u32,
            MoveProvenance {
                rel_path: "src/foo.rs".to_string(),
                line: 0,
            },
        )];
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &provenances,
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let suffix = build_row_suffix(Some(2), &paint);
        assert!(suffix.text.is_empty());
        assert!(suffix.runs.is_empty());
        let suffix = build_row_suffix(None, &paint);
        assert!(suffix.text.is_empty());
    }

    #[test]
    fn build_row_suffix_chip_uses_one_indexed_line_number() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot, false);
        let provenances = vec![(
            0u32,
            MoveProvenance {
                rel_path: "a.rs".to_string(),
                line: 0,
            },
        )];
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &provenances,
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let suffix = build_row_suffix(Some(0), &paint);
        assert_eq!(
            suffix.text, "  <- a.rs:1",
            "line 0 in the source displays as 1 (1-indexed) per TUI convention",
        );
    }

    fn deleted_after(line: u32) -> stoat::DiffHunk {
        stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Deleted,
            staged: false,
            buffer_start_line: line + 1,
            buffer_line_range: (line + 1)..(line + 1),
            base_byte_range: 0..1,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn added_rows(range: Range<u32>) -> stoat::DiffHunk {
        stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Added,
            staged: false,
            buffer_start_line: range.start,
            buffer_line_range: range,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    #[test]
    fn build_gutter_prefix_paints_caret_below_deletion() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map =
            stoat::DiffMap::from_hunks([deleted_after(0)], Some(Arc::new("gone\n".to_string())));
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(1, &paint);
        assert_eq!(diff_strip_char(&prefix, metrics.line_number_width), '^');
    }

    #[test]
    fn build_gutter_prefix_no_caret_on_first_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb");
        let diff_map =
            stoat::DiffMap::from_hunks([deleted_after(0)], Some(Arc::new("gone\n".to_string())));
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(0, &paint);
        assert_eq!(diff_strip_char(&prefix, metrics.line_number_width), ' ');
    }

    #[test]
    fn build_gutter_prefix_status_wins_over_caret() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "a\nb\nc");
        let diff_map = stoat::DiffMap::from_hunks(
            [deleted_after(0), added_rows(1..2)],
            Some(Arc::new("gone\n".to_string())),
        );
        let metrics = gutter_metrics(&snapshot, false);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
            review_chunk_markers: &[],
            review_move_provenances: &[],
            blame: None,
            metrics,
            line_number_color: rgb(0x808080).into(),
        };

        let prefix = build_gutter_prefix(1, &paint);
        assert_eq!(diff_strip_char(&prefix, metrics.line_number_width), '|');
    }

    use stoat_text::{Bias, SelectionGoal};

    fn cursor_at(snapshot: &DisplaySnapshot, offset: usize, id: usize) -> Selection<Anchor> {
        let anchor = snapshot.buffer_snapshot().anchor_at(offset, Bias::Left);
        Selection {
            id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        }
    }

    fn range_selection(
        snapshot: &DisplaySnapshot,
        start: usize,
        end: usize,
        reversed: bool,
        id: usize,
    ) -> Selection<Anchor> {
        let buffer = snapshot.buffer_snapshot();
        Selection {
            id,
            start: buffer.anchor_at(start, Bias::Left),
            end: buffer.anchor_at(end, Bias::Left),
            reversed,
            goal: SelectionGoal::None,
        }
    }

    fn rows_for(snapshot: &DisplaySnapshot) -> Vec<RenderedRow> {
        let total = snapshot.max_point().row + 1;
        build_rendered_rows(snapshot, 0..total)
    }

    #[test]
    fn column_to_byte_offset_ascii_is_identity() {
        assert_eq!(column_to_byte_offset("hello", 0), 0);
        assert_eq!(column_to_byte_offset("hello", 3), 3);
        assert_eq!(column_to_byte_offset("hello", 5), 5);
    }

    #[test]
    fn column_to_byte_offset_utf8_multibyte_uses_chars() {
        // "wXrld" where X is a 2-byte UTF-8 char.
        let text = "w\u{00f8}rld";
        assert_eq!(column_to_byte_offset(text, 0), 0);
        assert_eq!(column_to_byte_offset(text, 1), 1);
        assert_eq!(column_to_byte_offset(text, 2), 3);
        assert_eq!(column_to_byte_offset(text, 3), 4);
        assert_eq!(column_to_byte_offset(text, 4), 5);
    }

    #[test]
    fn column_to_byte_offset_past_end_returns_text_len() {
        assert_eq!(column_to_byte_offset("abc", 99), 3);
        assert_eq!(column_to_byte_offset("", 0), 0);
        assert_eq!(column_to_byte_offset("", 5), 0);
    }

    #[test]
    fn compute_selection_paint_empty_selection_records_cursor_only() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello");
        let rows = rows_for(&snapshot);
        let sel = cursor_at(&snapshot, 2, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 0);
        assert!(paint.row_selection_spans.is_empty());
        assert_eq!(paint.row_cursors.get(&0), Some(&vec![2usize]));
    }

    #[test]
    fn compute_selection_paint_range_within_one_row_records_span() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello world");
        let rows = rows_for(&snapshot);
        let sel = range_selection(&snapshot, 2, 7, false, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 0);
        assert_eq!(paint.row_selection_spans.get(&0), Some(&vec![2..7]));
        assert_eq!(paint.row_cursors.get(&0), Some(&vec![7usize]));
    }

    #[test]
    fn compute_selection_paint_range_spanning_rows_records_per_row_spans() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha\nbeta\ngamma");
        let rows = rows_for(&snapshot);
        // Offsets: 1 = 'l' on row 0, 8 = 'a' on row 1 (alpha\nbet|a).
        let sel = range_selection(&snapshot, 1, 8, false, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 0);
        assert_eq!(paint.row_selection_spans.get(&0), Some(&vec![1..5]));
        assert_eq!(paint.row_selection_spans.get(&1), Some(&vec![0..2]));
        assert_eq!(paint.row_selection_spans.get(&2), None);
        assert_eq!(paint.row_cursors.get(&1), Some(&vec![2usize]));
    }

    #[test]
    fn compute_selection_paint_reversed_selection_head_at_start() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello world");
        let rows = rows_for(&snapshot);
        let sel = range_selection(&snapshot, 2, 7, true, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 0);
        assert_eq!(paint.row_selection_spans.get(&0), Some(&vec![2..7]));
        assert_eq!(paint.row_cursors.get(&0), Some(&vec![2usize]));
    }

    #[test]
    fn compute_selection_paint_skips_rows_outside_visible_range() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "row0\nrow1\nrow2\nrow3");
        let rows = build_rendered_rows(&snapshot, 1..3);
        // Selection on row 0 must not be recorded when start_row is 1.
        let sel = range_selection(&snapshot, 0, 3, false, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 1);
        assert!(paint.row_selection_spans.is_empty());
        assert!(paint.row_cursors.is_empty());
        assert_eq!(paint.active_line_row, None);
    }

    #[test]
    fn compute_selection_paint_records_primary_head_display_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha\nbeta\ngamma");
        let rows = rows_for(&snapshot);
        let sel = cursor_at(&snapshot, 8, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 0);
        assert_eq!(paint.active_line_row, Some(1));
    }

    #[test]
    fn compute_selection_paint_active_line_follows_highest_id_selection() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha\nbeta\ngamma");
        let rows = rows_for(&snapshot);
        let secondary = cursor_at(&snapshot, 0, 1);
        let primary = cursor_at(&snapshot, 8, 5);

        let paint = compute_selection_paint(&snapshot, &[secondary, primary], &rows, 0);
        assert_eq!(paint.active_line_row, Some(1));
    }

    #[test]
    fn compute_selection_paint_active_line_unset_when_primary_offscreen() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "row0\nrow1\nrow2\nrow3");
        let rows = build_rendered_rows(&snapshot, 2..4);
        let sel = cursor_at(&snapshot, 1, 1);

        let paint = compute_selection_paint(&snapshot, &[sel], &rows, 2);
        assert_eq!(paint.active_line_row, None);
    }

    fn assert_merged_paint_valid(row: &RenderedRow) {
        let text_len = row.text.as_ref().len();
        let mut prev_end = 0;
        let mut sum = 0;
        for (range, _) in &row.runs {
            assert!(
                range.start >= prev_end,
                "runs must be sorted and non-overlapping; got {range:?} after end {prev_end}",
            );
            assert!(
                range.end > range.start,
                "no zero-length runs allowed; got {range:?}",
            );
            assert!(
                range.end <= text_len,
                "run {range:?} spills past text len {text_len}",
            );
            sum += range.end - range.start;
            prev_end = range.end;
        }
        assert_eq!(sum, text_len, "merged runs must cover the full text length",);
    }

    fn style_at(row: &RenderedRow, byte: usize) -> gpui::HighlightStyle {
        row.runs
            .iter()
            .find(|(r, _)| r.start <= byte && byte < r.end)
            .map(|(_, s)| *s)
            .unwrap_or_else(|| panic!("no run covers byte {byte}"))
    }

    #[test]
    fn apply_selection_paint_adds_background_run_for_span() {
        let mut paint = SelectionPaint::default();
        paint.row_selection_spans.insert(0, vec![1..4]);
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), "hello");
        assert_merged_paint_valid(&painted);
        assert_eq!(style_at(&painted, 0).background_color, None);
        assert_eq!(
            style_at(&painted, 1).background_color,
            Some(selection_color)
        );
        assert_eq!(
            style_at(&painted, 3).background_color,
            Some(selection_color)
        );
        assert_eq!(style_at(&painted, 4).background_color, None);
    }

    #[test]
    fn apply_selection_paint_appends_space_for_eol_cursor() {
        let mut paint = SelectionPaint::default();
        paint.row_cursors.insert(0, vec![5]);
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), "hello ");
        assert_merged_paint_valid(&painted);
        assert_eq!(style_at(&painted, 0).background_color, None);
        assert_eq!(style_at(&painted, 5).background_color, Some(cursor_color));
    }

    #[test]
    fn apply_selection_paint_cursor_overrides_selection_at_head() {
        let mut paint = SelectionPaint::default();
        paint.row_selection_spans.insert(0, vec![0..5]);
        paint.row_cursors.insert(0, vec![2]);
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), "hello");
        assert_merged_paint_valid(&painted);
        assert_eq!(
            style_at(&painted, 0).background_color,
            Some(selection_color)
        );
        assert_eq!(
            style_at(&painted, 1).background_color,
            Some(selection_color)
        );
        assert_eq!(style_at(&painted, 2).background_color, Some(cursor_color));
        assert_eq!(
            style_at(&painted, 3).background_color,
            Some(selection_color)
        );
        assert_eq!(
            style_at(&painted, 4).background_color,
            Some(selection_color)
        );
    }

    #[test]
    fn apply_selection_paint_paints_active_line_row_wide() {
        let paint = SelectionPaint {
            active_line_row: Some(0),
            ..Default::default()
        };
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), "hello");
        assert_merged_paint_valid(&painted);
        for byte in 0..5 {
            assert_eq!(
                style_at(&painted, byte).background_color,
                Some(active_line_color),
                "byte {byte} should carry active_line_color",
            );
        }
    }

    #[test]
    fn apply_selection_paint_skips_active_line_on_other_rows() {
        let paint = SelectionPaint {
            active_line_row: Some(1),
            ..Default::default()
        };
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), "hello");
        // No overlay touches row 0, so the fast path returns the input
        // runs (empty) unchanged. gpui's compute_runs fills the gap
        // with the default text style at paint time.
        assert!(painted.runs.is_empty());
    }

    #[test]
    fn apply_selection_paint_fast_path_preserves_shared_string_pointer() {
        let paint = SelectionPaint::default();
        let original = SharedString::from("hello");
        let original_ptr = original.as_ref().as_ptr();
        let row = RenderedRow {
            text: original,
            runs: Vec::new(),
        };
        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            hsla(0.0, 0.0, 0.0, 0.0),
            rgb(0x000000).into(),
            rgb(0xffffff).into(),
            rgb(0x000000).into(),
        );
        assert_eq!(
            painted.text.as_ref().as_ptr(),
            original_ptr,
            "fast path must move the SharedString through without reallocating",
        );
        assert!(painted.runs.is_empty());
    }

    #[test]
    fn apply_selection_paint_active_line_paints_under_selection_and_cursor() {
        let mut paint = SelectionPaint {
            active_line_row: Some(0),
            ..Default::default()
        };
        paint.row_selection_spans.insert(0, vec![0..3]);
        paint.row_cursors.insert(0, vec![3]);
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_merged_paint_valid(&painted);
        assert_eq!(
            style_at(&painted, 0).background_color,
            Some(selection_color)
        );
        assert_eq!(
            style_at(&painted, 2).background_color,
            Some(selection_color)
        );
        assert_eq!(style_at(&painted, 3).background_color, Some(cursor_color));
        assert_eq!(
            style_at(&painted, 4).background_color,
            Some(active_line_color)
        );
    }

    #[test]
    fn apply_selection_paint_active_line_appends_space_on_empty_row() {
        let paint = SelectionPaint {
            active_line_row: Some(0),
            ..Default::default()
        };
        let row = RenderedRow {
            text: SharedString::from(""),
            runs: Vec::new(),
        };
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color = rgb(0xc8d6ff).into();
        let cursor_text_color = rgb(0x101010).into();
        let active_line_color = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );
        assert_eq!(painted.text.as_ref(), " ");
        assert_merged_paint_valid(&painted);
        assert_eq!(
            style_at(&painted, 0).background_color,
            Some(active_line_color)
        );
    }

    #[test]
    fn apply_selection_paint_merges_syntax_with_overlays() {
        let syntax_color: Hsla = rgb(0xff8800).into();
        let syntax_style = gpui::HighlightStyle {
            color: Some(syntax_color),
            ..Default::default()
        };
        let row = RenderedRow {
            text: SharedString::from("hello"),
            runs: vec![(0..5, syntax_style)],
        };
        let mut paint = SelectionPaint::default();
        paint.row_selection_spans.insert(0, vec![1..4]);
        paint.row_cursors.insert(0, vec![2]);
        let selection_color = hsla(0.6, 0.5, 0.5, 0.3);
        let cursor_color: Hsla = rgb(0xc8d6ff).into();
        let cursor_text_color: Hsla = rgb(0x101010).into();
        let active_line_color: Hsla = rgb(0x2a2a2a).into();

        let painted = apply_selection_paint(
            row,
            0,
            &paint,
            selection_color,
            cursor_color,
            cursor_text_color,
            active_line_color,
        );

        assert_eq!(painted.text.as_ref(), "hello");
        assert_merged_paint_valid(&painted);
        // Syntax foreground color survives outside the cursor segment.
        for byte in [0usize, 1, 3, 4] {
            assert_eq!(
                style_at(&painted, byte).color,
                Some(syntax_color),
                "syntax color must survive merge at byte {byte}",
            );
        }
        // Background priority: cursor > selection > syntax.
        assert_eq!(style_at(&painted, 0).background_color, None);
        assert_eq!(
            style_at(&painted, 1).background_color,
            Some(selection_color)
        );
        assert_eq!(style_at(&painted, 2).background_color, Some(cursor_color));
        assert_eq!(
            style_at(&painted, 3).background_color,
            Some(selection_color)
        );
        assert_eq!(style_at(&painted, 4).background_color, None);
        // Cursor foreground overrides syntax color so the glyph reads
        // against the cursor block (reverse-video).
        assert_eq!(style_at(&painted, 2).color, Some(cursor_text_color));
    }

    fn snapshot_with_block(
        cx: &mut TestAppContext,
        text: &str,
        placement: stoat::display_map::BlockPlacement,
        block_lines: Vec<String>,
    ) -> DisplaySnapshot {
        use stoat::display_map::{BlockProperties, BlockStyle};
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let buffer_id = buffer.read_with(cx, |b, _| b.read(|tb| tb.buffer_id()));
        let shared = buffer.read_with(cx, |b, _| b.shared().clone());
        let multi_buffer = stoat::MultiBuffer::singleton(buffer_id, shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut inner = stoat::DisplayMap::new(multi_buffer, executor);
        inner.insert_blocks(vec![BlockProperties::from_text(
            placement,
            block_lines,
            BlockStyle::Fixed,
        )]);
        inner.snapshot()
    }

    #[test]
    fn block_rows_render_block_text() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_block(
            &mut cx,
            "alpha\nbeta",
            stoat::display_map::BlockPlacement::Above(0),
            vec!["block-line-1".into(), "block-line-2".into()],
        );

        let total = snapshot.max_point().row + 1;
        let rows = build_rendered_rows(&snapshot, 0..total);

        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_ref()).collect();
        assert_eq!(texts, vec!["block-line-1", "block-line-2", "alpha", "beta"]);
    }

    #[test]
    fn buffer_rows_remain_unchanged_when_block_inserted() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_block(
            &mut cx,
            "alpha\nbeta\ngamma",
            stoat::display_map::BlockPlacement::Above(1),
            vec!["divider".into()],
        );

        let total = snapshot.max_point().row + 1;
        let rows = build_rendered_rows(&snapshot, 0..total);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_ref()).collect();
        assert_eq!(texts, vec!["alpha", "divider", "beta", "gamma"]);
    }

    #[test]
    fn block_row_text_carries_block_style_run() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_block(
            &mut cx,
            "alpha",
            stoat::display_map::BlockPlacement::Above(0),
            vec!["header".into()],
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "header");
        assert_eq!(rows[0].runs.len(), 1);
        assert_eq!(rows[0].runs[0].0, 0..6);
        assert_eq!(rows[0].runs[0].1.color.map(hex_of), Some(BLOCK_TEXT_HEX),);
    }

    fn snapshot_with_render_block(
        cx: &mut TestAppContext,
        text: &str,
        placement: stoat::display_map::BlockPlacement,
        height: u32,
        render: stoat::display_map::RenderBlock,
    ) -> DisplaySnapshot {
        use stoat::display_map::{BlockProperties, BlockStyle};
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let buffer_id = buffer.read_with(cx, |b, _| b.read(|tb| tb.buffer_id()));
        let shared = buffer.read_with(cx, |b, _| b.shared().clone());
        let multi_buffer = stoat::MultiBuffer::singleton(buffer_id, shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut inner = stoat::DisplayMap::new(multi_buffer, executor);
        inner.insert_blocks(vec![BlockProperties {
            placement,
            height: Some(height),
            style: BlockStyle::Fixed,
            render,
            diff_status: None,
            priority: 0,
        }]);
        inner.snapshot()
    }

    #[test]
    fn block_spans_with_color_become_highlight_runs() {
        use ratatui::text::{Line, Span};
        let mut cx = TestAppContext::single();
        let render: stoat::display_map::RenderBlock = Arc::new(|_ctx| {
            vec![Line::from(vec![
                Span::styled("red", RatatuiStyle::new().fg(Color::Red)),
                Span::raw("plain"),
            ])]
        });
        let snapshot = snapshot_with_render_block(
            &mut cx,
            "alpha",
            stoat::display_map::BlockPlacement::Above(0),
            1,
            render,
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "redplain");
        assert_eq!(rows[0].runs.len(), 2);
        assert_eq!(rows[0].runs[0].0, 0..3);
        assert_eq!(rows[0].runs[0].1.color.map(hex_of), Some(0xcd0000));
        assert_eq!(rows[0].runs[1].0, 3..8);
        assert_eq!(rows[0].runs[1].1.color.map(hex_of), Some(BLOCK_TEXT_HEX));
    }

    #[test]
    fn block_span_modifier_maps_to_font_attribute() {
        use ratatui::text::{Line, Span};
        let mut cx = TestAppContext::single();
        let render: stoat::display_map::RenderBlock = Arc::new(|_ctx| {
            vec![Line::from(vec![Span::styled(
                "bold",
                RatatuiStyle::new().add_modifier(Modifier::BOLD),
            )])]
        });
        let snapshot = snapshot_with_render_block(
            &mut cx,
            "alpha",
            stoat::display_map::BlockPlacement::Above(0),
            1,
            render,
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "bold");
        assert_eq!(rows[0].runs.len(), 1);
        assert_eq!(rows[0].runs[0].1.font_weight, Some(FontWeight::BOLD));
    }

    fn snapshot_with_diff_block(
        cx: &mut TestAppContext,
        text: &str,
        block_lines: Vec<String>,
        diff_status: stoat::DiffHunkStatus,
    ) -> DisplaySnapshot {
        use stoat::display_map::{BlockPlacement, BlockProperties, BlockStyle};
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let buffer_id = buffer.read_with(cx, |b, _| b.read(|tb| tb.buffer_id()));
        let shared = buffer.read_with(cx, |b, _| b.shared().clone());
        let multi_buffer = stoat::MultiBuffer::singleton(buffer_id, shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut inner = stoat::DisplayMap::new(multi_buffer, executor);
        let mut props =
            BlockProperties::from_text(BlockPlacement::Above(0), block_lines, BlockStyle::Fixed);
        props.diff_status = Some(diff_status);
        inner.insert_blocks(vec![props]);
        inner.snapshot()
    }

    #[test]
    fn block_with_deleted_diff_status_paints_red() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_diff_block(
            &mut cx,
            "alpha",
            vec!["gone".into()],
            stoat::DiffHunkStatus::Deleted,
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "gone");
        assert_eq!(rows[0].runs.len(), 1);
        assert_eq!(rows[0].runs[0].1.color.map(hex_of), Some(DIFF_DELETED_HEX));
    }

    #[test]
    fn block_with_modified_diff_status_paints_red() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_diff_block(
            &mut cx,
            "alpha",
            vec!["replaced".into()],
            stoat::DiffHunkStatus::Modified,
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "replaced");
        assert_eq!(rows[0].runs.len(), 1);
        assert_eq!(rows[0].runs[0].1.color.map(hex_of), Some(DIFF_DELETED_HEX));
    }

    #[test]
    fn block_with_moved_diff_status_paints_cyan() {
        let mut cx = TestAppContext::single();
        let snapshot = snapshot_with_diff_block(
            &mut cx,
            "alpha",
            vec!["relocated".into()],
            stoat::DiffHunkStatus::Moved,
        );

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows[0].text.as_ref(), "relocated");
        assert_eq!(rows[0].runs.len(), 1);
        assert_eq!(rows[0].runs[0].1.color.map(hex_of), Some(DIFF_MOVED_HEX));
    }

    fn snapshot_with_diff_map(
        cx: &mut TestAppContext,
        text: &str,
        diff_map: stoat::DiffMap,
    ) -> DisplaySnapshot {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let buffer_id = buffer.read_with(cx, |b, _| b.read(|tb| tb.buffer_id()));
        let shared = buffer.read_with(cx, |b, _| b.shared().clone());
        {
            let mut guard = shared.write().expect("buffer lock poisoned");
            guard.diff_map = Some(diff_map);
        }
        let multi_buffer = stoat::MultiBuffer::singleton(buffer_id, shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut inner = stoat::DisplayMap::new(multi_buffer, executor);
        inner.snapshot()
    }

    fn detail(
        buffer_spans: Vec<stoat::ChangeSpan>,
        base_spans: Vec<stoat::ChangeSpan>,
    ) -> stoat::TokenDetail {
        stoat::TokenDetail {
            buffer_spans,
            base_spans,
        }
    }

    fn span(byte_range: Range<usize>, kind: stoat::ChangeKind) -> stoat::ChangeSpan {
        stoat::ChangeSpan {
            byte_range,
            kind,
            move_metadata: None,
        }
    }

    fn underline_run(
        runs: &[(Range<usize>, gpui::HighlightStyle)],
    ) -> &(Range<usize>, gpui::HighlightStyle) {
        runs.iter()
            .find(|(_, style)| style.underline.is_some())
            .expect("underline run")
    }

    fn underline_color(style: &gpui::HighlightStyle) -> Option<u32> {
        style.underline.as_ref().and_then(|u| u.color).map(hex_of)
    }

    #[test]
    fn move_chip_overlay_appends_basename_chip_for_cross_file_move() {
        use stoat_language::structural_diff::{BufferRef, MoveMetadata, MoveSource, Side};
        let mut cx = TestAppContext::single();
        let source = MoveSource {
            buffer: Some(BufferRef {
                path: std::path::PathBuf::from("/repo/src/other.rs"),
                fingerprint: [0u8; 32],
            }),
            side: Side::Lhs,
            byte_range: 0..5,
            line_range: 3..4,
        };
        let metadata = Arc::new(MoveMetadata {
            sources: vec![source],
        });
        let moved_span = stoat::ChangeSpan {
            byte_range: 0..5,
            kind: stoat::ChangeKind::Moved,
            move_metadata: Some(metadata),
        };
        let hunk = stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Moved,
            staged: false,
            buffer_start_line: 0,
            buffer_line_range: 0..1,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(Arc::new(detail(vec![moved_span], Vec::new()))),
        };
        let diff_map = stoat::DiffMap::from_hunks([hunk], None);
        let snapshot = snapshot_with_diff_map(&mut cx, "hello", diff_map);

        let mut rows = build_rendered_rows(&snapshot, 0..1);
        apply_move_chip_overlay(&mut rows, &snapshot, 0..1);

        assert_eq!(rows[0].text.as_ref(), "hello  <- other.rs:4");
        let chip_run = rows[0]
            .runs
            .iter()
            .find(|(range, _)| range.start == 5 && range.end == "hello  <- other.rs:4".len())
            .expect("chip run present");
        assert_eq!(chip_run.1.color.map(hex_of), Some(DIFF_MOVED_HEX));
    }

    #[test]
    fn move_chip_overlay_skips_intra_file_move() {
        use stoat_language::structural_diff::{MoveMetadata, MoveSource, Side};
        let mut cx = TestAppContext::single();
        let source = MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 0..5,
            line_range: 3..4,
        };
        let metadata = Arc::new(MoveMetadata {
            sources: vec![source],
        });
        let moved_span = stoat::ChangeSpan {
            byte_range: 0..5,
            kind: stoat::ChangeKind::Moved,
            move_metadata: Some(metadata),
        };
        let hunk = stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Moved,
            staged: false,
            buffer_start_line: 0,
            buffer_line_range: 0..1,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(Arc::new(detail(vec![moved_span], Vec::new()))),
        };
        let diff_map = stoat::DiffMap::from_hunks([hunk], None);
        let snapshot = snapshot_with_diff_map(&mut cx, "hello", diff_map);

        let mut rows = build_rendered_rows(&snapshot, 0..1);
        apply_move_chip_overlay(&mut rows, &snapshot, 0..1);

        assert_eq!(
            rows[0].text.as_ref(),
            "hello",
            "intra-file moves must not produce a chip",
        );
    }

    #[test]
    fn token_overlay_paints_buffer_spans_as_underline() {
        let mut cx = TestAppContext::single();
        let hunk = stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Modified,
            staged: false,
            buffer_start_line: 1,
            buffer_line_range: 1..2,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(Arc::new(detail(
                vec![span(7..10, stoat::ChangeKind::Replaced)],
                Vec::new(),
            ))),
        };
        let diff_map = stoat::DiffMap::from_hunks([hunk], None);
        let snapshot = snapshot_with_diff_map(&mut cx, "hello\nworld", diff_map);

        let rows = build_rendered_rows(&snapshot, 0..2);
        assert_eq!(rows[1].text.as_ref(), "world");
        let overlay = underline_run(&rows[1].runs);
        assert_eq!(overlay.0, 1..4);
        assert_eq!(underline_color(&overlay.1), Some(DIFF_MODIFIED_HEX));
    }

    #[test]
    fn token_overlay_uses_moved_color_for_moved_kind() {
        let mut cx = TestAppContext::single();
        let hunk = stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Modified,
            staged: false,
            buffer_start_line: 0,
            buffer_line_range: 0..1,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(Arc::new(detail(
                vec![span(0..2, stoat::ChangeKind::Moved)],
                Vec::new(),
            ))),
        };
        let diff_map = stoat::DiffMap::from_hunks([hunk], None);
        let snapshot = snapshot_with_diff_map(&mut cx, "hi", diff_map);

        let rows = build_rendered_rows(&snapshot, 0..1);
        let overlay = underline_run(&rows[0].runs);
        assert_eq!(overlay.0, 0..2);
        assert_eq!(underline_color(&overlay.1), Some(DIFF_MOVED_HEX));
    }

    #[test]
    fn token_overlay_paints_base_spans_on_deleted_block() {
        let mut cx = TestAppContext::single();
        let base = "removed\n";
        let hunk = stoat::DiffHunk {
            status: stoat::DiffHunkStatus::Modified,
            staged: false,
            buffer_start_line: 1,
            buffer_line_range: 1..2,
            base_byte_range: 0..7,
            anchor_range: None,
            token_detail: Some(Arc::new(detail(
                Vec::new(),
                vec![span(0..3, stoat::ChangeKind::Replaced)],
            ))),
        };
        let diff_map = stoat::DiffMap::from_hunks([hunk], Some(Arc::new(base.to_string())));
        let snapshot = snapshot_with_diff_map(&mut cx, "kept\nstay", diff_map);

        let total = snapshot.max_point().row + 1;
        let rows = build_rendered_rows(&snapshot, 0..total);
        let block_row = rows
            .iter()
            .find(|r| r.text.as_ref() == "removed")
            .expect("deleted block row");
        let overlay = underline_run(&block_row.runs);
        assert_eq!(overlay.0, 0..3);
        assert_eq!(underline_color(&overlay.1), Some(DIFF_DELETED_HEX));
    }

    #[test]
    fn review_moved_overlay_paints_underline_at_matching_buffer_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha\nbeta\ngamma");
        let mut rows = build_rendered_rows(&snapshot, 0..3);
        let moved = vec![(1u32, 1..3)];

        apply_review_moved_overlay(&mut rows, &snapshot, 0..3, &moved);

        assert!(
            rows[0].runs.is_empty(),
            "row 0 has no matching span and stays untouched"
        );
        let overlay = underline_run(&rows[1].runs);
        assert_eq!(overlay.0, 1..3);
        assert_eq!(underline_color(&overlay.1), Some(DIFF_MOVED_HEX));
        assert!(
            rows[2].runs.is_empty(),
            "row 2 has no matching span and stays untouched"
        );
    }

    #[test]
    fn review_moved_overlay_clamps_span_exceeding_row_length() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hi");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_review_moved_overlay(&mut rows, &snapshot, 0..1, &[(0, 0..99)]);

        let overlay = underline_run(&rows[0].runs);
        assert_eq!(overlay.0, 0..2);
    }

    #[test]
    fn review_moved_overlay_is_noop_when_spans_empty() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_review_moved_overlay(&mut rows, &snapshot, 0..1, &[]);

        assert!(rows[0].runs.is_empty());
    }

    #[test]
    fn review_moved_overlay_skips_span_with_no_matching_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_review_moved_overlay(&mut rows, &snapshot, 0..1, &[(5, 0..2)]);

        assert!(rows[0].runs.is_empty());
    }

    fn search_overlay_color() -> Hsla {
        hsla(0.1, 0.5, 0.5, 1.0)
    }

    fn bg_runs(runs: &[(Range<usize>, gpui::HighlightStyle)]) -> Vec<(Range<usize>, Option<Hsla>)> {
        runs.iter()
            .filter(|(_, s)| s.background_color.is_some())
            .map(|(r, s)| (r.clone(), s.background_color))
            .collect()
    }

    fn search_with_maps(
        rows: &mut [RenderedRow],
        snapshot: &DisplaySnapshot,
        range: Range<u32>,
        query: &str,
        color: Hsla,
    ) {
        if query.is_empty() {
            return;
        }
        let Ok(regex) = stoat::action_handlers::search::compile_search_regex(query) else {
            return;
        };
        let maps = build_row_byte_maps(rows, snapshot, range.clone());
        apply_search_overlay(rows, &maps, snapshot, range, &regex, color);
    }

    #[test]
    fn search_overlay_paints_single_match() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc def abc");
        let mut rows = build_rendered_rows(&snapshot, 0..1);
        let color = search_overlay_color();

        search_with_maps(&mut rows, &snapshot, 0..1, "def", color);

        assert_eq!(bg_runs(&rows[0].runs), vec![(4..7, Some(color))]);
    }

    #[test]
    fn search_overlay_paints_multiple_matches_on_same_row() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc abc abc");
        let mut rows = build_rendered_rows(&snapshot, 0..1);
        let color = search_overlay_color();

        search_with_maps(&mut rows, &snapshot, 0..1, "abc", color);

        assert_eq!(
            bg_runs(&rows[0].runs),
            vec![
                (0..3, Some(color)),
                (4..7, Some(color)),
                (8..11, Some(color)),
            ]
        );
    }

    #[test]
    fn search_overlay_paints_matches_across_rows() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc\nabc");
        let mut rows = build_rendered_rows(&snapshot, 0..2);
        let color = search_overlay_color();

        search_with_maps(&mut rows, &snapshot, 0..2, "abc", color);

        assert_eq!(bg_runs(&rows[0].runs), vec![(0..3, Some(color))]);
        assert_eq!(bg_runs(&rows[1].runs), vec![(0..3, Some(color))]);
    }

    #[test]
    fn search_overlay_skips_matches_outside_visible_range() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc\ndef\nabc");
        let mut rows = build_rendered_rows(&snapshot, 0..2);
        let color = search_overlay_color();

        search_with_maps(&mut rows, &snapshot, 0..2, "abc", color);

        assert_eq!(bg_runs(&rows[0].runs), vec![(0..3, Some(color))]);
        assert!(bg_runs(&rows[1].runs).is_empty());
    }

    #[test]
    fn search_overlay_handles_regex_anchors() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "xfoo\nfoo");
        let mut rows = build_rendered_rows(&snapshot, 0..2);
        let color = search_overlay_color();

        search_with_maps(&mut rows, &snapshot, 0..2, "^foo", color);

        assert!(bg_runs(&rows[0].runs).is_empty());
        assert_eq!(bg_runs(&rows[1].runs), vec![(0..3, Some(color))]);
    }

    #[test]
    fn search_overlay_empty_query_is_noop() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        search_with_maps(&mut rows, &snapshot, 0..1, "", search_overlay_color());

        assert!(bg_runs(&rows[0].runs).is_empty());
    }

    #[test]
    fn search_overlay_invalid_regex_is_noop() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        search_with_maps(
            &mut rows,
            &snapshot,
            0..1,
            "[unclosed",
            search_overlay_color(),
        );

        assert!(bg_runs(&rows[0].runs).is_empty());
    }

    #[test]
    fn search_overlay_zero_width_match_is_noop() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abc");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        search_with_maps(&mut rows, &snapshot, 0..1, "^", search_overlay_color());

        assert!(bg_runs(&rows[0].runs).is_empty());
    }

    fn fg_runs(runs: &[(Range<usize>, gpui::HighlightStyle)]) -> Vec<(Range<usize>, Option<Hsla>)> {
        runs.iter()
            .filter(|(_, s)| s.color.is_some())
            .map(|(r, s)| (r.clone(), s.color))
            .collect()
    }

    fn install_rust_with_themed_keywords() -> (
        Arc<stoat_language::Language>,
        stoat::display_map::syntax_theme::SyntaxStyles,
    ) {
        let registry = stoat_language::LanguageRegistry::standard();
        let language = registry
            .languages()
            .iter()
            .find(|l| l.name == "rust")
            .expect("rust language registered")
            .clone();
        let theme = {
            use stoat_config::parse;
            let (config, _) = parse("theme t { syntax.keyword.fg = blue; }");
            stoat::theme::Theme::from_config(&config.expect("parse"), "t").expect("theme")
        };
        let styles = stoat::display_map::syntax_theme::SyntaxStyles::from_theme(&theme);
        let map = stoat_language::HighlightMap::new(
            language.highlight_capture_names(),
            styles.theme_keys(),
        );
        language.set_highlight_map(map);
        (language, styles)
    }

    fn build_rust_syntax_snapshot(text: &str) -> stoat_language::SyntaxSnapshot {
        let (language, _styles) = install_rust_with_themed_keywords();
        let rope = stoat_text::Rope::from(text);
        let mut map = stoat_language::SyntaxMap::new();
        let _ = map.reparse(&rope, language, 1);
        map.snapshot().clone()
    }

    #[test]
    fn syntax_overlay_paints_keyword() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "fn main() {}");
        let mut rows = build_rendered_rows(&snapshot, 0..1);
        let syntax = build_rust_syntax_snapshot("fn main() {}");
        let (_, styles) = install_rust_with_themed_keywords();

        let maps = build_row_byte_maps(&rows, &snapshot, 0..1);
        apply_syntax_overlay(&mut rows, &maps, &snapshot, 0..1, &syntax, &styles);

        let runs = fg_runs(&rows[0].runs);
        let keyword_run = runs
            .iter()
            .find(|(r, _)| r.start == 0 && r.end == 2)
            .expect("`fn` keyword run");
        assert!(keyword_run.1.is_some(), "keyword should have a color");
    }

    #[test]
    fn syntax_overlay_empty_snapshot_is_noop() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "fn main() {}");
        let mut rows = build_rendered_rows(&snapshot, 0..1);
        let empty = stoat_language::SyntaxSnapshot::default();
        let (_, styles) = install_rust_with_themed_keywords();

        let maps = build_row_byte_maps(&rows, &snapshot, 0..1);
        apply_syntax_overlay(&mut rows, &maps, &snapshot, 0..1, &empty, &styles);

        assert!(rows[0].runs.is_empty());
    }

    #[test]
    fn syntax_overlay_skips_captures_outside_visible_range() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "let x = 1;\nfn main() {}");
        let mut rows = build_rendered_rows(&snapshot, 0..1);
        let syntax = build_rust_syntax_snapshot("let x = 1;\nfn main() {}");
        let (_, styles) = install_rust_with_themed_keywords();

        let maps = build_row_byte_maps(&rows, &snapshot, 0..1);
        apply_syntax_overlay(&mut rows, &maps, &snapshot, 0..1, &syntax, &styles);

        let runs = fg_runs(&rows[0].runs);
        assert!(
            !runs.iter().any(|(r, _)| r.start >= 11),
            "row 0 should not paint runs whose offset belongs to row 1"
        );
    }

    #[test]
    fn syntax_overlay_paints_across_visible_rows() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "fn a() {}\nfn b() {}");
        let mut rows = build_rendered_rows(&snapshot, 0..2);
        let syntax = build_rust_syntax_snapshot("fn a() {}\nfn b() {}");
        let (_, styles) = install_rust_with_themed_keywords();

        let maps = build_row_byte_maps(&rows, &snapshot, 0..2);
        apply_syntax_overlay(&mut rows, &maps, &snapshot, 0..2, &syntax, &styles);

        let row0 = fg_runs(&rows[0].runs);
        let row1 = fg_runs(&rows[1].runs);
        assert!(
            row0.iter().any(|(r, _)| r.start == 0 && r.end == 2),
            "row 0 should paint `fn`"
        );
        assert!(
            row1.iter().any(|(r, _)| r.start == 0 && r.end == 2),
            "row 1 should paint `fn`"
        );
    }

    fn label_color() -> Hsla {
        hsla(0.15, 1.0, 0.5, 1.0)
    }

    fn prefix_color() -> Hsla {
        hsla(0.15, 1.0, 0.25, 1.0)
    }

    fn labels(entries: &[(&str, usize)]) -> std::collections::BTreeMap<String, usize> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn goto_word_overlay_paints_single_char_label_at_target_column() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha beta");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[("a", 0), ("b", 6)]),
            "",
            label_color(),
            prefix_color(),
        );

        let runs = bg_runs(&rows[0].runs);
        assert_eq!(runs.len(), 2);
        let first = runs
            .iter()
            .find(|(r, _)| r.start == 0)
            .expect("label at offset 0");
        assert_eq!(first.0.end - first.0.start, 1);
        let second = runs
            .iter()
            .find(|(r, _)| r.start == 6)
            .expect("label at offset 6");
        assert_eq!(second.0.end - second.0.start, 1);
        let row_text: &str = rows[0].text.as_ref();
        assert_eq!(&row_text[0..1], "a");
        assert_eq!(&row_text[6..7], "b");
    }

    #[test]
    fn goto_word_overlay_paints_two_char_label() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abcdef");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[("xy", 0)]),
            "",
            label_color(),
            prefix_color(),
        );

        let row_text: &str = rows[0].text.as_ref();
        assert_eq!(&row_text[0..2], "xy");
        let runs = bg_runs(&rows[0].runs);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn goto_word_overlay_dim_color_for_prefix_match() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "abcdef");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[("xy", 0)]),
            "x",
            label_color(),
            prefix_color(),
        );

        let runs = bg_runs(&rows[0].runs);
        let prefix = runs
            .iter()
            .find(|(r, _)| r.start == 0 && r.end == 1)
            .expect("prefix run");
        assert_eq!(prefix.1, Some(prefix_color()));
        let remaining = runs
            .iter()
            .find(|(r, _)| r.start == 1 && r.end == 2)
            .expect("remaining run");
        assert_eq!(remaining.1, Some(label_color()));
    }

    #[test]
    fn goto_word_overlay_skips_label_not_matching_prefix() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha beta");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[("a", 0), ("b", 6)]),
            "a",
            label_color(),
            prefix_color(),
        );

        let runs = bg_runs(&rows[0].runs);
        assert_eq!(runs.len(), 1, "only the prefix-matching label paints");
        assert_eq!(runs[0].0, 0..1);
    }

    #[test]
    fn goto_word_overlay_skips_label_outside_visible_range() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha\nbeta");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[("a", 6)]),
            "",
            label_color(),
            prefix_color(),
        );

        assert!(bg_runs(&rows[0].runs).is_empty());
    }

    #[test]
    fn goto_word_overlay_empty_labels_is_noop() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "alpha");
        let mut rows = build_rendered_rows(&snapshot, 0..1);

        apply_goto_word_overlay(
            &mut rows,
            &snapshot,
            0..1,
            &labels(&[]),
            "",
            label_color(),
            prefix_color(),
        );

        assert!(bg_runs(&rows[0].runs).is_empty());
    }
}
