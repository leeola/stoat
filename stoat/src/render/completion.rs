use crate::{
    app::Stoat,
    completion::CompletionItem,
    fuzzy,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use nucleo::Utf32Str;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders, Clear, Widget},
};
use std::ops::Range;

/// Maximum number of completion rows visible at once. Larger lists
/// scroll so the selected row stays in view.
pub(crate) const MAX_VISIBLE_ROWS: usize = 10;

/// The on-screen rectangles of the completion popup, derived from `stoat` by
/// [`completion_popup_layout`], plus the scroll offset of the first visible row.
///
/// The popup is cursor-anchored, so the rects move with the cursor each frame.
/// Shared by the renderer and the smooth-scroll emit so the pooled list region
/// matches the painted one exactly.
pub(crate) struct CompletionLayout {
    /// The bordered popup rect.
    pub(crate) popup_area: Rect,
    /// Inside the border: the scrolling row region, also the pool region.
    /// Excludes the detail footer row when one is present.
    pub(crate) inner: Rect,
    /// Index of the first visible item, the list's scroll offset.
    pub(crate) viewport_top: usize,
    /// Detail-and-documentation line for the selected row, painted dimmed
    /// in a footer row just below [`Self::inner`]. `None` when the
    /// selected row has neither.
    pub(crate) detail: Option<String>,
}

/// Compute the anchored completion popup geometry, returning its match prefix
/// and the [`CompletionLayout`], or `None` when no popup should show.
///
/// The popup itself is not returned. Callers re-borrow
/// [`Stoat::pending_completion`] for the item rows, so the list is never cloned
/// here. `None` mirrors every [`render_completion`] bail: no pending completion
/// or empty items, the focused pane is not an editor, the cursor is off-screen,
/// or the interior width collapses to zero.
pub(crate) fn completion_popup_layout(stoat: &mut Stoat) -> Option<(String, CompletionLayout)> {
    // Read the scalars the geometry needs and drop the popup borrow before the
    // mutable workspace access below. The item list is re-borrowed for the width
    // scan once that work is done, so the popup is never cloned.
    let (anchor_offset, selected_idx, prefix_range) = match &stoat.pending_completion {
        Some(p) if !p.items.is_empty() => (p.anchor_offset, p.selected_idx, p.prefix_range.clone()),
        _ => return None,
    };

    let prefix = extract_prefix(stoat, prefix_range);

    let (cursor_screen, content_area) = {
        let ws = stoat.active_workspace_mut();
        let FocusTarget::SplitPane = ws.focus else {
            return None;
        };
        let pane_id = ws.panes.focus();
        let pane = ws.panes.pane(pane_id);
        let View::Editor(editor_id) = pane.view else {
            return None;
        };
        let (content_area, _) = split_pane_status(pane.area);
        let editor = ws.editors.get_mut(editor_id)?;
        let cursor_screen = cursor_screen_position(editor, content_area, anchor_offset)?;
        (cursor_screen, content_area)
    };

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return None;
    }

    let popup = stoat.pending_completion.as_ref()?;
    let total = popup.items.len();
    let viewport_top = viewport_top_for(selected_idx, total, MAX_VISIBLE_ROWS);
    let visible_count = total.saturating_sub(viewport_top).min(MAX_VISIBLE_ROWS);

    let max_line_width = popup
        .items
        .iter()
        .skip(viewport_top)
        .take(visible_count)
        .map(|item| {
            truncate_to_width(&item.label, interior_width as usize)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0) as u16;
    let detail = popup.items.get(selected_idx).and_then(detail_footer);
    let detail_width = detail
        .as_deref()
        .map(|d| {
            truncate_to_width(d, interior_width as usize)
                .chars()
                .count()
        })
        .unwrap_or(0) as u16;
    let footer_rows: u16 = if detail.is_some() { 1 } else { 0 };

    let popup_width = (max_line_width.max(detail_width) + 2).clamp(3, content_area.width.max(3));
    let popup_height =
        (visible_count as u16 + footer_rows + 2).clamp(3, content_area.height.max(3));

    let popup_x = cursor_screen
        .0
        .min(content_area.x + content_area.width.saturating_sub(popup_width));
    let popup_y = if cursor_screen.1 + 1 + popup_height <= content_area.y + content_area.height {
        cursor_screen.1 + 1
    } else if cursor_screen.1 >= content_area.y + popup_height {
        cursor_screen.1 - popup_height
    } else {
        content_area.y
    };

    let popup_area = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };
    let full_inner = Block::default().borders(Borders::ALL).inner(popup_area);
    let inner = Rect {
        height: full_inner.height.saturating_sub(footer_rows),
        ..full_inner
    };

    Some((
        prefix,
        CompletionLayout {
            popup_area,
            inner,
            viewport_top,
            detail,
        },
    ))
}

/// The dimmed footer line for a completion row, joining its detail with
/// the first line of its documentation. `None` when it has neither.
/// Painted just below the list by [`render_completion`].
fn detail_footer(item: &CompletionItem) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(detail) = item.detail.as_deref().filter(|d| !d.is_empty()) {
        parts.push(detail);
    }
    if let Some(first) = item
        .documentation
        .as_deref()
        .and_then(|doc| doc.lines().next())
        .filter(|line| !line.is_empty())
    {
        parts.push(first);
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// Paint the completion popup, if any, anchored to the focused
/// editor's primary cursor. Renders below the cursor when there is
/// room below, otherwise above. Truncates labels that exceed the
/// popup's interior width and clamps height to the focused pane.
///
/// No-op when [`Stoat::pending_completion`] is `None` or empty,
/// when the focused pane is not an editor, when the cursor is
/// off-screen, or when neither the popup width nor height fits.
pub(crate) fn render_completion(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let Some((prefix, layout)) = completion_popup_layout(stoat) else {
        return;
    };
    // The layout confirmed a non-empty popup. Re-borrow it for the rows instead
    // of cloning the item list through the layout call.
    let Some(popup) = stoat.pending_completion.as_ref() else {
        return;
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);
    Clear.render(layout.popup_area, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.popup_area,
        None,
        modal_style,
        &stoat.theme,
        scene,
    );

    paint_completion_rows(
        &popup.items,
        popup.selected_idx,
        &prefix,
        layout.viewport_top,
        layout.inner,
        &stoat.theme,
        buf,
    );

    if let Some(detail) = &layout.detail {
        let footer_y = layout.inner.y + layout.inner.height;
        let footer_style = modal_style.add_modifier(Modifier::DIM);
        let text = truncate_to_width(detail, layout.inner.width as usize);
        for (col_idx, ch) in text.chars().enumerate() {
            let col = layout.inner.x + col_idx as u16;
            if col >= layout.inner.x + layout.inner.width {
                break;
            }
            buf[(col, footer_y)].set_char(ch).set_style(footer_style);
        }
    }
}

/// Paint completion rows into `area` starting at item `start_row`, one row per
/// item, truncating each label to the area width and highlighting the selected
/// row and the characters matching `prefix`.
///
/// Shared by the live popup, which derives `start_row` from the viewport, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows.
pub(crate) fn paint_completion_rows(
    items: &[CompletionItem],
    selected_idx: usize,
    prefix: &str,
    start_row: usize,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let pattern = fuzzy::parse_query(prefix);
    let mut matcher_guard = pattern
        .as_ref()
        .map(|_| fuzzy::matcher().lock().expect("fuzzy matcher poisoned"));
    let mut hay_buf: Vec<char> = Vec::new();
    let mut indices_buf: Vec<u32> = Vec::new();

    let width = area.width as usize;
    for row_idx in 0..area.height {
        let item_idx = start_row + row_idx as usize;
        let Some(item) = items.get(item_idx) else {
            break;
        };
        let row = area.y + row_idx;
        let label = truncate_to_width(&item.label, width);
        let row_style = if item_idx == selected_idx {
            selected_style
        } else {
            modal_style
        };

        indices_buf.clear();
        if let (Some(p), Some(matcher)) = (&pattern, matcher_guard.as_deref_mut()) {
            let hay = Utf32Str::new(&label, &mut hay_buf);
            p.indices(hay, matcher, &mut indices_buf);
        }

        for (col_idx, ch) in label.chars().enumerate() {
            let col = area.x + col_idx as u16;
            if col >= area.x + area.width {
                break;
            }
            let style = if indices_buf.contains(&(col_idx as u32)) {
                match_style
            } else {
                row_style
            };
            buf[(col, row)].set_char(ch).set_style(style);
        }
    }
}

fn extract_prefix(stoat: &Stoat, prefix_range: Range<usize>) -> String {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane = ws.focus else {
        return String::new();
    };
    let pane_id = ws.panes.focus();
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return String::new();
    };
    let Some(editor) = ws.editors.get(editor_id) else {
        return String::new();
    };
    let Some(buffer) = ws.buffers.get(editor.buffer_id) else {
        return String::new();
    };
    let guard = match buffer.read() {
        Ok(g) => g,
        Err(_) => return String::new(),
    };
    let rope = guard.rope();
    let len = rope.len();
    let start = prefix_range.start.min(len);
    let end = prefix_range.end.min(len);
    if start >= end {
        return String::new();
    }
    rope.slice(start..end).to_string()
}

fn viewport_top_for(selected: usize, total: usize, window: usize) -> usize {
    if total <= window {
        return 0;
    }
    let max_top = total - window;
    if selected < window {
        0
    } else {
        (selected + 1).saturating_sub(window).min(max_top)
    }
}

fn cursor_screen_position(
    editor: &mut crate::editor_state::EditorState,
    content_area: Rect,
    anchor_offset: usize,
) -> Option<(u16, u16)> {
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    if anchor_offset > rope.len() {
        return None;
    }
    let point = rope.offset_to_point(anchor_offset);
    let display = snapshot.buffer_to_display(point);
    if display.row < editor.scroll_row {
        return None;
    }
    let visible_rows = content_area.height as u32;
    if display.row >= editor.scroll_row + visible_rows {
        return None;
    }
    let y = content_area.y + (display.row - editor.scroll_row) as u16;
    let x = content_area.x + display.column as u16;
    if x >= content_area.x + content_area.width || y >= content_area.y + content_area.height {
        return None;
    }
    Some((x, y))
}

fn truncate_to_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use crate::{
        completion::{CompletionItem, CompletionPopup, CompletionSource},
        test_harness::TestHarness,
    };
    use std::path::PathBuf;
    use stoat_action as action;

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.txt");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        crate::action_handlers::dispatch(&mut h.stoat, &action::OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn make_item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: 0..0,
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        }
    }

    #[test]
    fn snapshot_popup_basic_three_items_selected_middle() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![make_item("println"), make_item("print"), make_item("panic")],
            selected_idx: 1,
            anchor_offset: 0,
            prefix_range: 0..0,
        });
        h.assert_snapshot("snapshot_completion_popup_basic");
    }

    #[test]
    fn snapshot_popup_shows_detail_footer_for_selected_row() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch(&mut h, "");
        h.type_keys("i");
        let mut foo = make_item("foo");
        foo.detail = Some("fn foo() -> u32".into());
        foo.documentation = Some("Returns the foo.\nMore details.".into());
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![foo, make_item("bar")],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
        });
        h.assert_snapshot("snapshot_completion_popup_detail_footer");
    }

    #[test]
    fn snapshot_popup_scrolls_when_selected_past_window() {
        let mut h = TestHarness::with_size(40, 16);
        let _path = open_scratch(&mut h, "");
        h.type_keys("i");
        let items: Vec<CompletionItem> = (0..15)
            .map(|i| make_item(&format!("item_{i:02}")))
            .collect();
        h.stoat.pending_completion = Some(CompletionPopup {
            items,
            selected_idx: 12,
            anchor_offset: 0,
            prefix_range: 0..0,
        });
        h.assert_snapshot("snapshot_completion_popup_scrolling");
    }

    #[test]
    fn snapshot_popup_highlights_matched_chars() {
        let mut h = TestHarness::with_size(40, 12);
        let _path = open_scratch(&mut h, "pri");
        h.type_keys("A");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![make_item("println"), make_item("print"), make_item("panic")],
            selected_idx: 0,
            anchor_offset: 3,
            prefix_range: 0..3,
        });
        h.assert_snapshot("snapshot_completion_popup_with_match");
    }

    #[test]
    fn no_popup_state_renders_no_paint_block() {
        let mut h = TestHarness::with_size(40, 6);
        let _path = open_scratch(&mut h, "");
        h.type_keys("i");
        assert!(h.stoat.pending_completion.is_none());
        h.assert_snapshot("snapshot_completion_popup_absent");
    }
}
