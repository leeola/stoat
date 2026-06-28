use crate::{
    file_finder::{display_row, FileFinder, FinderScope},
    host::FsHost,
    render::{
        editor::render_editor,
        text::{write_str, write_str_clipped},
    },
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};

/// The on-screen rectangles of the file finder modal, derived from a terminal
/// `area` by [`file_finder_layout`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list region
/// matches the painted one exactly.
pub(crate) struct FinderLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: prompt, input, separator, and body.
    pub(crate) inner: Rect,
    /// The result list, also the smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The preview pane, present only when the body is wide enough.
    pub(crate) preview: Option<Rect>,
}

/// Lay out the file finder modal within `area`, or `None` when `area` is too
/// small to host it.
pub(crate) fn file_finder_layout(area: Rect) -> Option<FinderLayout> {
    if area.width < 40 || area.height < 12 {
        return None;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 32u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return None;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    // The title rides the top border, so it does not shrink the inner rect.
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    let body_width = inner.width;

    let (list, preview) = if body_width >= 80 {
        let list_width = (body_width * 40 / 100).max(24);
        let preview_width = body_width.saturating_sub(list_width + 1);
        (
            Rect::new(inner.x, body_top, list_width, body_height),
            Some(Rect::new(
                inner.x + list_width + 1,
                body_top,
                preview_width,
                body_height,
            )),
        )
    } else {
        (Rect::new(inner.x, body_top, body_width, body_height), None)
    };

    Some(FinderLayout {
        modal,
        inner,
        list,
        preview,
    })
}

pub(crate) fn render_file_finder(
    finder: &mut FileFinder,
    ws: &mut Workspace,
    fs_host: &dyn FsHost,
    language_registry: &stoat_language::LanguageRegistry,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    finder.refilter_from_input(ws);
    finder.sync_preview(ws, fs_host, language_registry);

    let Some(layout) = file_finder_layout(area) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = match finder.scope() {
        FinderScope::All => " file finder (all) ",
        FinderScope::Modified => " file finder (modified) ",
        FinderScope::Buffers => " file finder (buffers) ",
    };
    Clear.render(layout.modal, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    block.render(layout.modal, buf);

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ">", prompt_style);
    let input_area = Rect::new(inner.x + 2, input_row, inner.width.saturating_sub(2), 1);
    finder.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    let separator_row = inner.y + 1;
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(muted_style);
    }

    if let Some(preview_rect) = layout.preview {
        for row in layout.list.y..layout.list.y + layout.list.height {
            buf[(layout.list.x + layout.list.width, row)]
                .set_char('│')
                .set_style(muted_style);
        }
        render_preview(finder, preview_rect, theme, ws, buf);
    }

    finder.viewport_rows = Some(layout.list.height as usize);
    render_list(finder, layout.list, theme, buf);
}

fn render_list(finder: &FileFinder, area: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let rows = area.height as usize;
    let start_row = finder.selected.saturating_sub(rows.saturating_sub(1));
    paint_finder_rows(finder, area, start_row, theme, buf);
}

/// Paint finder result rows into `area` starting at `start_row`, one row per
/// line, with the selected row and fuzzy-match characters highlighted.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows.
pub(crate) fn paint_finder_rows(
    finder: &FileFinder,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let base = finder.base_paths();
    let end_x = area.x + area.width;
    let label_x = area.x + 1;

    for (row_idx, (&idx, indices)) in finder
        .filtered
        .iter()
        .zip(finder.match_indices.iter())
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let row = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == finder.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..end_x {
            buf[(col, row)].set_char(' ').set_style(style);
        }
        let path = &base[idx];
        let label = display_row(path, &finder.git_root);
        write_str_clipped(buf, label_x, row, &label, style, end_x);
        for (label_col, _) in label.chars().enumerate() {
            let col = label_x + label_col as u16;
            if col >= end_x {
                break;
            }
            if indices.binary_search(&(label_col as u32)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

fn render_preview(
    finder: &FileFinder,
    area: Rect,
    theme: &crate::theme::Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fallback = theme.get(crate::theme::scope::UI_TEXT);
    if let Some(editor) = ws.editors.get_mut(finder.preview_editor) {
        render_editor(editor, area, fallback, theme, buf, false);
    }
}
