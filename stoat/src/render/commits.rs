use crate::{
    commit_list::{CommitListState, Preview},
    diff_map::{BaseHighlights, ChangeKind},
    host::{CommitFileChange, CommitFileChangeKind},
    pane::Pane,
    render::{
        layout::split_pane_status,
        paint::{render_empty_num, render_side_num, render_side_text},
        pane::render_overlay_status,
        review::{paint_base_row, resolve_diff_tints, DiffColumns, DiffLayout, DiffTints},
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
    review::{ReviewRow, ReviewSide},
    review_session::DiffDocument,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use std::path::Path;

pub(crate) fn render_commits(
    pane: &Pane,
    is_focused: bool,
    state: &mut CommitListState,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let theme = frame.theme;
    let workspace_root = frame.workspace_root;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, buf, &mut *scene);

    let Some(left_area) = commits_list_rect(pane.area) else {
        return;
    };
    let sep_x = left_area.x + left_area.width;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_area.width + 1);

    let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    crate::render::chrome::vline(buf, sep_x, inner.y, inner.height, sep_style, &mut *scene);

    state.viewport_rows = left_area.height as usize;
    state.ensure_selected_visible(state.viewport_rows);
    render_commit_list_pane(state, theme, left_area, buf);

    if right_w > 0 {
        let right_area = Rect::new(right_x, inner.y, right_w, inner.height);
        render_commit_detail_pane(state, workspace_root, theme, right_area, buf, scene);
    }
}

/// The commit list's rectangle within an overlay pane, or `None` when the pane
/// is too small. Shared by the renderer and the smooth-scroll emit so the pooled
/// region matches the painted list.
pub(crate) fn commits_list_rect(pane_area: Rect) -> Option<Rect> {
    let (inner, _) = split_pane_status(pane_area);
    if inner.width < 10 || inner.height == 0 {
        return None;
    }
    let left_w = commit_list_width(inner.width);
    Some(Rect::new(inner.x, inner.y, left_w, inner.height))
}

fn commit_list_width(total: u16) -> u16 {
    let target = (total as u32 * 2 / 5) as u16;
    target.clamp(22, 48).min(total.saturating_sub(12))
}

fn render_commit_list_pane(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    let top = state.scroll_top.min(state.commits.len().saturating_sub(1));
    paint_commit_rows(state, area, top, theme, buf);
}

/// Paint the commit list into `area` starting at row `start_row`, with the
/// selected row highlighted and a trailing load/end marker.
///
/// Shared by the live list, which derives `start_row` from `scroll_top`, and the
/// smooth-scroll pool, which paints absolute pages, so both render identical
/// rows.
pub(crate) fn paint_commit_rows(
    state: &CommitListState,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::VCS_COMMIT_METADATA);
    if state.commits.is_empty() {
        let msg = if state.pending_load.is_some() {
            "loading commits..."
        } else {
            "no commits"
        };
        write_str(buf, area.x, area.y, msg, dim);
        return;
    }

    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let sha_style = theme.get(s::VCS_COMMIT_SHA);
    let summary_style = theme.get(s::VCS_COMMIT_SUMMARY);

    let top = start_row.min(state.commits.len());
    let rows_visible = area.height as usize;
    let end = (top + rows_visible).min(state.commits.len());

    for (i, commit) in state.commits[top..end].iter().enumerate() {
        let y = area.y + i as u16;
        let is_selected = top + i == state.selected;
        let row_style = if is_selected {
            sel_style
        } else {
            summary_style
        };

        if is_selected && area.width > 0 {
            for x in area.x..area.x + area.width {
                buf[(x, y)].set_style(sel_style);
            }
        }

        let sha_x = area.x;
        let sha = &commit.short_sha;
        let sha_len = sha.len().min(area.width as usize);
        write_str(
            buf,
            sha_x,
            y,
            &sha[..sha_len],
            if is_selected { sel_style } else { sha_style },
        );

        let summary_x = sha_x + sha_len as u16 + 1;
        let remaining = (area.x + area.width).saturating_sub(summary_x);
        if remaining > 0 {
            let summary = truncate_to_cols(&commit.summary, remaining as usize);
            write_str(buf, summary_x, y, &summary, row_style);
        }
    }

    if state.pending_load.is_some() && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "loading more...", dim);
    } else if state.reached_end && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "(end of history)", dim);
    }
}

fn render_commit_detail_pane(
    state: &CommitListState,
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let dim = theme.get(crate::theme::scope::VCS_COMMIT_METADATA);
    let Some(sha) = state.selected_sha() else {
        write_str(buf, area.x, area.y, "no selection", dim);
        return;
    };

    let summary_rows = match state.summaries.get(sha) {
        Some(changes) => render_commit_summary(changes, workspace_root, theme, area, buf),
        None => {
            write_str(buf, area.x, area.y, "loading summary...", dim);
            1
        },
    };

    let preview_y = area.y + summary_rows as u16 + 1;
    if preview_y >= area.y + area.height {
        return;
    }
    let preview_area = Rect::new(
        area.x,
        preview_y,
        area.width,
        area.y + area.height - preview_y,
    );
    match state.preview_sessions.get(sha) {
        // The commits view has no preview scroll of its own. Only the picker's
        // wheel handling moves one.
        Preview::Built(document) => {
            render_commit_preview(document, theme, preview_area, 0, buf, scene)
        },
        Preview::Empty => {
            if preview_area.height > 0 {
                write_str(buf, preview_area.x, preview_area.y, "no changes", dim);
            }
        },
        Preview::Unbuilt => {
            if preview_area.height > 0 {
                write_str(
                    buf,
                    preview_area.x,
                    preview_area.y,
                    "loading preview...",
                    dim,
                );
            }
        },
    }
}

fn render_commit_summary(
    changes: &[CommitFileChange],
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) -> usize {
    use crate::theme::scope as s;
    let header_style = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    let path_style = theme.get(s::UI_TEXT);
    let add_style = theme.get(s::DIFF_ADDED);
    let del_style = theme.get(s::DIFF_DELETED);

    let total_add: u32 = changes.iter().map(|c| c.additions).sum();
    let total_del: u32 = changes.iter().map(|c| c.deletions).sum();
    let header = format!(
        "{} file{}, +{total_add} -{total_del}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    write_str(buf, area.x, area.y, &header, header_style);

    let mut rows_used = 1;
    let max_rows = (area.height as usize).saturating_sub(1);
    for (i, change) in changes.iter().take(max_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        let kind_char = match change.kind {
            CommitFileChangeKind::Added => 'A',
            CommitFileChangeKind::Modified => 'M',
            CommitFileChangeKind::Deleted => 'D',
            CommitFileChangeKind::Renamed => 'R',
            CommitFileChangeKind::TypeChange => 'T',
        };
        write_str(buf, area.x, y, &format!("{kind_char} "), path_style);
        let rel = crate::paths::display_relative(&change.rel_path, workspace_root);
        let path_width = area.width.saturating_sub(2 + 12) as usize;
        let rel = truncate_to_cols(&rel, path_width);
        write_str(buf, area.x + 2, y, &rel, path_style);

        let stats = format!(" +{} -{}", change.additions, change.deletions);
        let stats_x = area.x + area.width.saturating_sub(stats.len() as u16);
        let split = stats.find('-').unwrap_or(stats.len());
        write_str(buf, stats_x, y, &stats[..split], add_style);
        write_str(buf, stats_x + split as u16, y, &stats[split..], del_style);
        rows_used += 1;
    }
    rows_used
}

/// Rows [`render_commit_preview`] would paint for `session` in full, counting
/// each chunk's header alongside its diff rows.
///
/// A caller's `skip_rows` is measured in these, so this is what bounds it.
pub(crate) fn preview_row_count(doc: &DiffDocument) -> usize {
    doc.files
        .iter()
        .flat_map(|file| file.chunks.iter())
        .filter_map(|chunk_id| doc.chunks.get(chunk_id))
        .map(|chunk| 1 + chunk.hunk.rows.len())
        .sum()
}

/// One side of a preview row, painted the way `:diff`'s base column paints.
///
/// Parity with `:diff` comes from calling its painter, not from copying its
/// treatment, so the two surfaces cannot drift.
///
/// `spans` is reused across rows rather than allocated per row, and is left
/// sorted for the painter's monotonic cursor.
#[allow(clippy::too_many_arguments)]
fn paint_preview_side(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    side: &ReviewSide,
    width: usize,
    highlights: Option<&BaseHighlights>,
    tints: &DiffTints,
    context: bool,
    spans: &mut Vec<(std::ops::Range<usize>, ChangeKind, bool)>,
) {
    spans.clear();
    // A [`ReviewSide`] carries plain ranges, so no span here claims prose. The
    // session serializes its rows, and the flag would have to cross that format
    // to reach the preview.
    spans.extend(
        side.change_spans
            .iter()
            .map(|range| (range.clone(), ChangeKind::Replaced, false)),
    );
    spans.extend(
        side.moved_spans
            .iter()
            .map(|range| (range.clone(), ChangeKind::Moved, false)),
    );
    spans.sort_by_key(|(range, ..)| range.start);

    // line_num is 1-based, and the highlight index is 0-based per line.
    let token_spans = highlights
        .and_then(|lines| lines.get(side.line_num.saturating_sub(1) as usize))
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // A context row recedes whole. A changed row stays at full strength and
    // instead softens only the chars outside its spans, so the refinement
    // leads its own line.
    let (soften_row, soften_gaps) = match context {
        true => (Some(tints.bg), None),
        false => (None, Some(tints.bg)),
    };
    paint_base_row(
        buf,
        x,
        y,
        &side.text,
        width,
        token_spans,
        Style::default(),
        spans,
        Some(tints),
        soften_row,
        soften_gaps,
        // The commit preview is its own screen, so neither the diff view's
        // soften knob nor its tint dial reaches it. The fractions stay as
        // shipped and a changed span keeps its syntax color.
        1.0,
        0.0,
        None,
    );
}
/// Render a compact preview of a [`DiffDocument`]: each chunk's rows
/// painted sequentially with a yellow file/chunk header, top-to-bottom
/// within `area`. Does not rely on editor machinery; used by the
/// commits view's right pane.
///
/// `skip_rows` drops that many rows off the top, counting the same rows
/// [`preview_row_count`] does, so a caller scrolls the diff by holding a row
/// offset rather than by owning any of the painting.
pub(crate) fn render_commit_preview(
    doc: &DiffDocument,
    theme: &crate::theme::Theme,
    area: Rect,
    skip_rows: usize,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_COMMIT_SHA);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let fallback_style = Style::default();
    // A non-RGB theme cannot blend, so it keeps the fg-span marking below.
    let tints = resolve_diff_tints(theme);

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();
    // Reused across rows the way num_text is, since every row rebuilds it.
    let mut spans: Vec<(std::ops::Range<usize>, ChangeKind, bool)> = Vec::new();

    let DiffColumns {
        left_num_x,
        left_text_x,
        left_content_w,
        right_num_x,
        right_text_x,
        right_content_w,
        sep_x,
        ..
    } = DiffColumns::compute(area, DiffLayout::REVIEW);

    let mut y = area.y;
    let end_y = area.y + area.height;
    // Counts every row the full preview would hold, painted or skipped, so the
    // caller's scroll lands on the same row it would have reached by counting
    // what it can see.
    let mut row = 0usize;

    for file in &doc.files {
        for chunk_id in &file.chunks {
            let Some(chunk) = doc.chunks.get(chunk_id) else {
                continue;
            };
            let file_total = file.chunks.len();
            let lang_str = file
                .language
                .as_ref()
                .map(|l| l.name.to_string())
                .unwrap_or_default();
            let label = format!(
                "{} --- {}/{} --- {}",
                file.rel_path,
                chunk.chunk_index_in_file + 1,
                file_total,
                lang_str
            );
            let label_row = row;
            row += 1;
            if label_row >= skip_rows {
                if y >= end_y {
                    return;
                }
                let label_trunc = truncate_to_cols(&label, area.width as usize);
                write_str(buf, area.x, y, &label_trunc, header_style);
                y += 1;
            }

            for diff_row in &chunk.hunk.rows {
                let this_row = row;
                row += 1;
                if this_row < skip_rows {
                    continue;
                }
                if y >= end_y {
                    return;
                }
                if let Some(sep_x) = sep_x
                    && sep_x < area.x + area.width
                {
                    crate::render::chrome::vline(buf, sep_x, y, 1, dim, &mut *scene);
                }
                match diff_row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, &mut num_text, left_num_x, y, left.line_num, dim);
                        render_side_num(buf, &mut num_text, right_num_x, y, right.line_num, dim);
                        match &tints {
                            Some(tints) => {
                                paint_preview_side(
                                    buf,
                                    left_text_x,
                                    y,
                                    left,
                                    left_content_w,
                                    file.base_highlights.as_deref(),
                                    tints,
                                    true,
                                    &mut spans,
                                );
                                paint_preview_side(
                                    buf,
                                    right_text_x,
                                    y,
                                    right,
                                    right_content_w,
                                    file.buffer_highlights.as_deref(),
                                    tints,
                                    true,
                                    &mut spans,
                                );
                            },
                            None => {
                                render_side_text(
                                    buf,
                                    left_text_x,
                                    y,
                                    &left.text,
                                    left_content_w,
                                    fallback_style,
                                    &[],
                                    fallback_style,
                                    &[],
                                    move_hl,
                                );
                                render_side_text(
                                    buf,
                                    right_text_x,
                                    y,
                                    &right.text,
                                    right_content_w,
                                    fallback_style,
                                    &[],
                                    fallback_style,
                                    &[],
                                    move_hl,
                                );
                            },
                        }
                    },
                    ReviewRow::Changed { left, right } => {
                        match left {
                            Some(l) => {
                                render_side_num(buf, &mut num_text, left_num_x, y, l.line_num, dim);
                                match &tints {
                                    Some(tints) => paint_preview_side(
                                        buf,
                                        left_text_x,
                                        y,
                                        l,
                                        left_content_w,
                                        file.base_highlights.as_deref(),
                                        tints,
                                        false,
                                        &mut spans,
                                    ),
                                    None => render_side_text(
                                        buf,
                                        left_text_x,
                                        y,
                                        &l.text,
                                        left_content_w,
                                        fallback_style,
                                        &l.change_spans,
                                        del_hl,
                                        &l.moved_spans,
                                        move_hl,
                                    ),
                                }
                            },
                            None => render_empty_num(buf, left_num_x, y, dim),
                        }
                        match right {
                            Some(r) => {
                                render_side_num(
                                    buf,
                                    &mut num_text,
                                    right_num_x,
                                    y,
                                    r.line_num,
                                    dim,
                                );
                                match &tints {
                                    Some(tints) => paint_preview_side(
                                        buf,
                                        right_text_x,
                                        y,
                                        r,
                                        right_content_w,
                                        file.buffer_highlights.as_deref(),
                                        tints,
                                        false,
                                        &mut spans,
                                    ),
                                    None => render_side_text(
                                        buf,
                                        right_text_x,
                                        y,
                                        &r.text,
                                        right_content_w,
                                        fallback_style,
                                        &r.change_spans,
                                        add_hl,
                                        &r.moved_spans,
                                        move_hl,
                                    ),
                                }
                            },
                            None => render_empty_num(buf, right_num_x, y, dim),
                        }
                    },
                }
                y += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render_commit_preview;
    use crate::{
        display_map::highlights::HighlightStyle,
        render::{
            paint::dim_rgb,
            review::{CONTEXT_SOFTEN, MODIFIED_ROW_SOFTEN},
        },
        review::ReviewFileInput,
        review_session::DiffDocument,
        theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    use std::{path::PathBuf, sync::Arc};

    /// The same RGB theme the diff-view tests use, so the washes engage.
    fn rgb_theme() -> Theme {
        let src = r##"theme rgbtest {
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
            diff.modified.fg = "#ffff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            diff.staged.fg   = "#00ffff";
            diff.unstaged.fg = "#ff00ff";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    const TOKEN_FG: [u8; 3] = [0x11, 0x99, 0x44];
    const BG: [u8; 3] = [0x28, 0x2c, 0x34];

    /// A document over one file, with `highlights` covering the first `lines`
    /// lines of both sides at [`TOKEN_FG`] across the whole line.
    fn session(base: &str, buffer: &str, highlights: bool) -> DiffDocument {
        let mut doc = DiffDocument::default();
        doc.add_files(vec![ReviewFileInput {
            path: PathBuf::from("/w/a.rs"),
            rel_path: "a.rs".into(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }]);
        if highlights {
            let style = HighlightStyle {
                foreground: Some(Color::Rgb(TOKEN_FG[0], TOKEN_FG[1], TOKEN_FG[2])),
                ..HighlightStyle::default()
            };
            let spans_for = |text: &str| -> Arc<crate::diff_map::BaseHighlights> {
                Arc::new(
                    text.lines()
                        .map(|line| vec![(0..line.len(), style.clone())])
                        .collect(),
                )
            };
            let file = &mut doc.files[0];
            file.base_highlights = Some(spans_for(base));
            file.buffer_highlights = Some(spans_for(buffer));
        }
        doc
    }

    /// Render into a wide buffer so both columns fit, and hand back the grid.
    fn rendered(doc: &DiffDocument, theme: &Theme) -> Buffer {
        let area = Rect::new(0, 0, 120, 20);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        render_commit_preview(doc, theme, area, 0, &mut buf, &mut scene);
        buf
    }

    /// The first cell in `row` whose symbol is `ch`.
    fn cell_with(buf: &Buffer, row: u16, ch: &str) -> ratatui::buffer::Cell {
        let area = *buf.area();
        (area.x..area.x + area.width)
            .map(|x| buf[(x, row)].clone())
            .find(|cell| cell.symbol() == ch)
            .unwrap_or_else(|| panic!("no {ch:?} cell on row {row}"))
    }

    /// Context recedes so the changed rows around it lead, which is the whole
    /// reason the preview carries syntax colors rather than flat text.
    #[test]
    fn a_context_token_softens_toward_the_background() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let buf = rendered(&session, &rgb_theme());

        // Row 0 is the file header, so the first content row is 1.
        let cell = cell_with(&buf, 1, "c");
        let softened = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        assert_eq!(
            cell.style().fg,
            Some(Color::Rgb(softened[0], softened[1], softened[2])),
            "the context token carries its syntax color, softened"
        );
    }

    /// A changed span is the one thing on the row that recedes nowhere, and it
    /// carries no color behind it, which is what makes a commit diff read
    /// like `:diff`.
    #[test]
    fn a_changed_span_keeps_its_token_color_and_takes_no_wash() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let buf = rendered(&session, &rgb_theme());

        let cell = cell_with(&buf, 2, "n");
        assert_eq!(
            (cell.style().fg, cell.style().bg, cell.modifier),
            (
                Some(Color::Rgb(TOKEN_FG[0], TOKEN_FG[1], TOKEN_FG[2])),
                Some(Color::Reset),
                Modifier::empty()
            ),
            "the changed token keeps full-strength syntax color, no wash, and no underline"
        );
    }

    /// The preview marks a change by holding it at full strength while its
    /// neighbors recede, so a file the build never attached highlights to has
    /// nothing to mark with and paints flat.
    #[test]
    fn a_file_without_highlights_paints_no_marking_at_all() {
        let session = session("ctx\nold\n", "ctx\nnew\n", false);
        let buf = rendered(&session, &rgb_theme());

        let changed = cell_with(&buf, 2, "n");
        let gap = cell_with(&buf, 1, "c");
        assert_eq!(
            (changed.style().fg, gap.style().fg),
            (Some(Color::Reset), Some(Color::Reset)),
            "no color reaches the changed span or its receding neighbor"
        );
    }

    /// A theme whose diff colors are not RGB cannot blend, so the preview keeps
    /// marking changes by foreground exactly as it did before.
    #[test]
    fn a_non_rgb_theme_keeps_the_foreground_span_marking() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let theme = Theme::empty();
        let buf = rendered(&session, &theme);

        let cell = cell_with(&buf, 2, "n");
        assert!(
            !matches!(cell.style().bg, Some(Color::Rgb(..))),
            "nothing washes behind the span: {:?}",
            cell.style().bg
        );
        assert_ne!(
            cell.style().fg,
            Some(Color::Rgb(TOKEN_FG[0], TOKEN_FG[1], TOKEN_FG[2])),
            "and the token color never reaches the cell, so the marking is the theme's"
        );
    }

    /// An unchanged char on a row the refinement split softens by the lighter
    /// amount, so the changed chars lead their own line without the row
    /// receding like context.
    #[test]
    fn an_unchanged_char_on_a_refined_row_softens_lightly() {
        let session = session("ctx\nfoo_a\n", "ctx\nfoo_b\n", true);
        let buf = rendered(&session, &rgb_theme());

        let cell = cell_with(&buf, 2, "f");
        let softened = dim_rgb(TOKEN_FG, BG, MODIFIED_ROW_SOFTEN);
        assert_eq!(
            cell.style().fg,
            Some(Color::Rgb(softened[0], softened[1], softened[2])),
            "the unchanged prefix softens lightly, not to context strength"
        );
    }
}
