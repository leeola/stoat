use crate::diagnostics::DiagnosticSet;
use gpui::{
    div, px, rgb, Div, FontStyle, FontWeight, Hsla, ParentElement, Pixels, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle,
};
use lsp_types::DiagnosticSeverity;
use ratatui::style::{Color, Modifier, Style as RatatuiStyle};
use std::{collections::BTreeMap, ops::Range, path::Path};
use stoat::{
    display_map::{Block, BlockContext, BlockId, HighlightStyle as StoatHighlightStyle},
    BlockRowKind, DisplayPoint, DisplaySnapshot, MultiBufferSnapshot,
};
use stoat_text::{Anchor, Selection};

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
            let mut row_text = String::new();
            let mut row_runs: Vec<(Range<usize>, gpui::HighlightStyle)> = Vec::new();
            for span in line.spans {
                if span.content.is_empty() {
                    continue;
                }
                let start = row_text.len();
                row_text.push_str(span.content.as_ref());
                let end = row_text.len();
                row_runs.push((start..end, convert_block_span_style(&span.style)));
            }
            texts[idx] = row_text;
            runs[idx] = row_runs;
        }
    }

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

fn convert_block_span_style(style: &RatatuiStyle) -> gpui::HighlightStyle {
    let mut converted = convert_ratatui_style(style);
    if converted.color.is_none() {
        converted.color = Some(rgb(BLOCK_TEXT_HEX).into());
    }
    converted
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
) -> RenderedRow {
    let RenderedRow { text, mut runs } = row;
    let mut text_owned: String = text.as_ref().to_string();

    if let Some(spans) = paint.row_selection_spans.get(&display_row) {
        let style = gpui::HighlightStyle {
            background_color: Some(selection_color),
            ..Default::default()
        };
        for span in spans {
            runs.push((span.clone(), style));
        }
    }

    if let Some(cursors) = paint.row_cursors.get(&display_row) {
        let style = gpui::HighlightStyle {
            background_color: Some(cursor_color),
            ..Default::default()
        };
        for &offset in cursors {
            if offset < text_owned.len() {
                let after = text_owned[offset..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                runs.push((offset..offset + after, style));
            } else {
                let appended_start = text_owned.len();
                text_owned.push(' ');
                runs.push((appended_start..text_owned.len(), style));
            }
        }
    }

    RenderedRow {
        text: SharedString::from(text_owned),
        runs,
    }
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
const DIAG_ERROR_HEX: u32 = 0xe53935;
const DIAG_WARNING_HEX: u32 = 0xffb300;
const DIAG_INFO_HEX: u32 = 0x29b6f6;
const DIAG_HINT_HEX: u32 = 0x9e9e9e;
const BLOCK_TEXT_HEX: u32 = 0xa0a0a0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GutterMetrics {
    pub line_number_width: usize,
    pub total_width: usize,
}

pub(crate) fn gutter_metrics(snapshot: &DisplaySnapshot) -> GutterMetrics {
    let buffer_line_count = snapshot.buffer_line_count().max(1);
    let line_number_width = digit_count(buffer_line_count);
    GutterMetrics {
        line_number_width,
        total_width: line_number_width + 1 + 1 + 1,
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
    pub metrics: GutterMetrics,
    pub line_number_color: Hsla,
}

pub(crate) fn render_row_with_gutter(
    row: RenderedRow,
    display_row: u32,
    paint: &GutterPaint<'_>,
) -> Div {
    let prefix = build_gutter_prefix(display_row, paint);
    let RenderedRow {
        text: row_text,
        runs: mut row_runs,
    } = row;

    let prefix_len = prefix.text.len();
    let mut text = String::with_capacity(prefix_len + row_text.len());
    text.push_str(&prefix.text);
    text.push_str(&row_text);

    let mut runs: Vec<(Range<usize>, gpui::HighlightStyle)> = prefix
        .runs
        .into_iter()
        .map(|(range, style)| (range.start..range.end, style))
        .collect();
    for (range, style) in row_runs.drain(..) {
        runs.push(((range.start + prefix_len)..(range.end + prefix_len), style));
    }

    div().child(StyledText::new(SharedString::from(text)).with_highlights(runs))
}

struct GutterPrefix {
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
    let start = text.len();
    if let Some((ch, hex)) = diff_strip_for_status(diff_status) {
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

    text.push(' ');

    GutterPrefix { text, runs }
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

        let metrics = gutter_metrics(&snapshot);
        assert_eq!(metrics.line_number_width, 1);
        assert_eq!(metrics.total_width, 4);
    }

    #[test]
    fn gutter_metrics_for_hundred_lines_uses_digit_count() {
        let mut cx = TestAppContext::single();
        let text = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let snapshot = test_snapshot(&mut cx, &text);

        let metrics = gutter_metrics(&snapshot);
        assert_eq!(metrics.line_number_width, 3);
        assert_eq!(metrics.total_width, 6);
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
    fn render_row_with_gutter_paints_line_number() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello\nworld");
        let diff_map = stoat::DiffMap::default();
        let metrics = gutter_metrics(&snapshot);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
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
        let metrics = gutter_metrics(&snapshot);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
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
        let metrics = gutter_metrics(&snapshot);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: None,
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
        let metrics = gutter_metrics(&snapshot);
        let mut diagnostics: DiagnosticRowMap = BTreeMap::new();
        diagnostics.insert(0, DiagnosticSeverity::WARNING);
        let paint = GutterPaint {
            display_snapshot: &snapshot,
            diff_map: &diff_map,
            diagnostics: Some(&diagnostics),
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

        let painted = apply_selection_paint(row, 0, &paint, selection_color, cursor_color);
        assert_eq!(painted.text.as_ref(), "hello");
        assert_eq!(painted.runs.len(), 1);
        assert_eq!(painted.runs[0].0, 1..4);
        assert_eq!(painted.runs[0].1.background_color, Some(selection_color));
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

        let painted = apply_selection_paint(row, 0, &paint, selection_color, cursor_color);
        assert_eq!(painted.text.as_ref(), "hello ");
        assert_eq!(painted.runs.len(), 1);
        assert_eq!(painted.runs[0].0, 5..6);
        assert_eq!(painted.runs[0].1.background_color, Some(cursor_color));
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

        let painted = apply_selection_paint(row, 0, &paint, selection_color, cursor_color);
        assert_eq!(painted.text.as_ref(), "hello");
        // Selection run added first, cursor run appended after; StyledText paints in
        // order so the cursor highlight wins at byte 2.
        assert_eq!(painted.runs.len(), 2);
        assert_eq!(painted.runs[0].0, 0..5);
        assert_eq!(painted.runs[0].1.background_color, Some(selection_color));
        assert_eq!(painted.runs[1].0, 2..3);
        assert_eq!(painted.runs[1].1.background_color, Some(cursor_color));
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
}
