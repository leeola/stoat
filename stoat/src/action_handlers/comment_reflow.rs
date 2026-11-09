//! The `ReflowComment` action, which joins the wrapped lines of a line comment
//! block and gives the block to the formatter.
//!
//! A formatter wraps a comment line that runs past the line width, but it does
//! not join short lines back up. So a comment that an indent change left ragged
//! stays ragged. The join removes the line breaks inside each paragraph, and the
//! formatter wraps the result again.

use super::{lsp, movement};
use crate::{
    app::{Stoat, UpdateEffect},
    host::LanguageServerFeature,
    lsp::{hosts, session},
};
use std::{
    iter,
    ops::{Range, RangeInclusive},
};
use stoat_text::{cursor_offset, Point, Rope};

/// One row of a line comment block, read from just past its comment token.
struct CommentRow {
    /// The buffer offset just past the row's comment token.
    body_start: usize,
    /// The row's text from `body_start` to the end of its content.
    body: String,
}

/// Join the paragraphs and list items of the line comment block under the
/// cursor, then format the document.
///
/// The join is one undo step, and the formatter's edits land as a second one.
/// Without a formatting server the action edits nothing and reports why in the
/// status row. A cursor row with no line comment, or a block with nothing to
/// join, leaves the buffer as it is and sends no request.
pub(super) fn reflow_comment(stoat: &mut Stoat) -> UpdateEffect {
    let Some((editor_id, buffer_id)) = super::pane_editor_ids(stoat.active_workspace()) else {
        return UpdateEffect::None;
    };
    if hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Format).is_empty() {
        return session::report_lsp_unavailable(stoat, "format");
    }

    let ws = stoat.active_workspace_mut();
    let tokens = ws
        .buffers
        .language_for(buffer_id)
        .map_or(&[][..], |lang| lang.line_comments);
    let joins = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buffer = snapshot.buffer_snapshot();
        let rope = buffer.rope();
        let newest = editor.selections.newest_anchor().clone();
        let cursor = cursor_offset(
            rope,
            buffer.resolve_anchor(&newest.tail()),
            buffer.resolve_anchor(&newest.head()),
        );
        let Some(rows) = comment_block(rope, rope.offset_to_point(cursor).row, tokens) else {
            return UpdateEffect::None;
        };
        join_ranges(&rows)
    };
    if joins.is_empty() {
        return UpdateEffect::None;
    }

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> =
            joins.into_iter().rev().map(|range| (range, " ")).collect();
        guard.edit_batch(&batch);
    }
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let snapshot = editor.display_map.snapshot();
    editor
        .selections
        .transform(snapshot.buffer_snapshot(), |sel| sel.clone());

    lsp::format_document(stoat);
    UpdateEffect::Redraw
}

/// The rows of the line comment block that holds buffer row `row`, or `None`
/// when `row` holds no line comment.
///
/// A block is a run of rows that open with the same comment token, so a `///`
/// run and a `//` run beside it stay apart. A comment that trails code is in
/// no block, because its row opens with the code.
fn comment_block(rope: &Rope, row: u32, tokens: &[&'static str]) -> Option<Vec<CommentRow>> {
    let (token, cursor_row) = comment_row(rope, row, tokens)?;
    let same_token = |row: u32| {
        comment_row(rope, row, tokens)
            .filter(|(other, _)| *other == token)
            .map(|(_, comment)| comment)
    };
    let above: Vec<CommentRow> = (0..row).rev().map_while(same_token).collect();
    let below = (row + 1..=rope.max_point().row).map_while(same_token);
    Some(
        above
            .into_iter()
            .rev()
            .chain(iter::once(cursor_row))
            .chain(below)
            .collect(),
    )
}

/// The comment token that opens buffer row `row`, with the row's comment text,
/// or `None` when the row opens with no token of `tokens`.
fn comment_row(
    rope: &Rope,
    row: u32,
    tokens: &[&'static str],
) -> Option<(&'static str, CommentRow)> {
    let line_start = rope.point_to_offset(Point::new(row, 0));
    let content_end = movement::line_content_end(rope, row);
    let (first, token) = movement::line_comment_continues(rope, line_start, content_end, tokens)?;

    let body_start = first + token.len();
    let body = rope.chunks_in_range(body_start..content_end).collect();
    Some((token, CommentRow { body_start, body }))
}

/// The byte ranges whose replacement by one space joins each paragraph and list
/// item of `rows` onto one line.
///
/// A range runs from the end of one row's text to the start of the next row's
/// text, so it takes the line break, the indentation, and the comment token
/// between them. Markdown keeps some rows on lines of their own, and those
/// never join. They are blank comment lines, fenced and indented code, tables,
/// headings, rule lines, and link definitions.
///
/// A list item takes each prose row below it whose indent is between its
/// marker's column and three columns past its text column. The first row of a
/// paragraph takes each prose row below it whose indent is between its own
/// indent and three columns past it. A row that indents four or more columns
/// past the least indent of the block is indented code.
fn join_ranges(rows: &[CommentRow]) -> Vec<Range<usize>> {
    let floor = rows
        .iter()
        .filter(|row| !row.body.trim().is_empty())
        .map(|row| indent_width(&row.body))
        .min()
        .unwrap_or(0);

    let mut joins = Vec::new();
    let mut in_fence = false;
    let mut open: Option<(RangeInclusive<usize>, &CommentRow)> = None;
    for row in rows {
        let text = row.body.trim();
        let indent = indent_width(&row.body);

        if text.starts_with("```") || text.starts_with("~~~") {
            in_fence = !in_fence;
            open = None;
        } else if in_fence || text.is_empty() || keeps_its_line(text) {
            open = None;
        } else if let Some(marker_len) = list_marker_len(text) {
            open = Some((indent..=indent + marker_len + 3, row));
        } else if let Some((run, last)) = open.take().filter(|(run, _)| run.contains(&indent)) {
            let end = last.body_start + last.body.trim_end().len();
            let start = row.body_start + (row.body.len() - row.body.trim_start().len());
            joins.push(end..start);
            open = Some((run, row));
        } else if indent >= floor + 4 {
            open = None;
        } else {
            open = Some((indent..=indent + 3, row));
        }
    }
    joins
}

/// Whether markdown keeps `text` on a line of its own, as a table row, a
/// heading, a link definition, or a rule line.
fn keeps_its_line(text: &str) -> bool {
    text.starts_with('|')
        || text.starts_with('#')
        || (text.starts_with('[') && text.contains("]:"))
        || (text.len() >= 3 && text.chars().all(|ch| matches!(ch, '-' | '=' | '*' | '_')))
}

/// The length of the list marker that opens `text`, its space included, or
/// `None` when `text` opens no list item.
///
/// A bullet is `-`, `*`, or `+`. A number is ASCII digits closed by `.` or `)`.
fn list_marker_len(text: &str) -> Option<usize> {
    if ["- ", "* ", "+ "]
        .into_iter()
        .any(|bullet| text.starts_with(bullet))
    {
        return Some(2);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &text[digits..];
    let closed = rest.starts_with(". ") || rest.starts_with(") ");
    (digits > 0 && closed).then_some(digits + 2)
}

/// The count of spaces and tabs that open `body`.
fn indent_width(body: &str) -> usize {
    body.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .count()
}
