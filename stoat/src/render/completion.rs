use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Matcher, Utf32Str,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};
use std::sync::{Mutex, OnceLock};

/// Maximum number of completion rows visible at once. Larger lists
/// scroll so the selected row stays in view.
pub(crate) const MAX_VISIBLE_ROWS: usize = 10;

/// Paint the completion popup, if any, anchored to the focused
/// editor's primary cursor. Renders below the cursor when there is
/// room below, otherwise above. Truncates labels that exceed the
/// popup's interior width and clamps height to the focused pane.
///
/// No-op when [`Stoat::pending_completion`] is `None` or empty,
/// when the focused pane is not an editor, when the cursor is
/// off-screen, or when neither the popup width nor height fits.
pub(crate) fn render_completion(stoat: &mut Stoat, buf: &mut Buffer) {
    let popup = match &stoat.pending_completion {
        Some(p) if !p.items.is_empty() => p.clone(),
        _ => return,
    };

    let prefix = extract_prefix(stoat, &popup);

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return;
    };

    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return;
    };
    let pane_area = pane.area;
    let (content_area, _) = split_pane_status(pane_area);

    let editor = match ws.editors.get_mut(editor_id) {
        Some(e) => e,
        None => return,
    };

    let cursor_screen = match cursor_screen_position(editor, content_area, popup.anchor_offset) {
        Some(p) => p,
        None => return,
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let selected_style = stoat.theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = stoat.theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }
    let total = popup.items.len();
    let viewport_top = viewport_top_for(popup.selected_idx, total, MAX_VISIBLE_ROWS);
    let visible_count = total.saturating_sub(viewport_top).min(MAX_VISIBLE_ROWS);

    let labels: Vec<String> = popup
        .items
        .iter()
        .skip(viewport_top)
        .take(visible_count)
        .map(|item| truncate_to_width(&item.label, interior_width as usize))
        .collect();

    let max_line_width = labels.iter().map(|s| s.chars().count()).max().unwrap_or(0) as u16;
    let popup_width = (max_line_width + 2).clamp(3, content_area.width.max(3));
    let popup_height = (visible_count as u16 + 2).clamp(3, content_area.height.max(3));

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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style);
    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let pattern = (!prefix.is_empty())
        .then(|| Pattern::parse(&prefix, CaseMatching::Smart, Normalization::Smart))
        .filter(|p| !p.atoms.is_empty());
    let mut matcher_guard = pattern
        .as_ref()
        .map(|_| fuzzy_matcher().lock().expect("fuzzy matcher poisoned"));

    let mut hay_buf: Vec<char> = Vec::new();
    let mut indices_buf: Vec<u32> = Vec::new();

    for (row_idx, label) in labels.iter().enumerate() {
        let row = inner.y + row_idx as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let item_idx = viewport_top + row_idx;
        let row_style = if item_idx == popup.selected_idx {
            selected_style
        } else {
            modal_style
        };

        indices_buf.clear();
        if let (Some(p), Some(matcher)) = (&pattern, matcher_guard.as_deref_mut()) {
            let hay = Utf32Str::new(label, &mut hay_buf);
            p.indices(hay, matcher, &mut indices_buf);
        }

        for (col_idx, ch) in label.chars().enumerate() {
            let col = inner.x + col_idx as u16;
            if col >= inner.x + inner.width {
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

fn extract_prefix(stoat: &Stoat, popup: &crate::completion::CompletionPopup) -> String {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return String::new();
    };
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
    let start = popup.prefix_range.start.min(len);
    let end = popup.prefix_range.end.min(len);
    if start >= end {
        return String::new();
    }
    rope.slice(start..end).to_string()
}

fn fuzzy_matcher() -> &'static Mutex<Matcher> {
    static MATCHER: OnceLock<Mutex<Matcher>> = OnceLock::new();
    MATCHER.get_or_init(|| Mutex::new(Matcher::default()))
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
