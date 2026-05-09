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
    widgets::{Block, Borders, Widget},
};

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

    if area.width < 40 || area.height < 12 {
        return;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 32u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = match finder.scope() {
        FinderScope::All => " file finder (all) ",
        FinderScope::Modified => " file finder (modified) ",
        FinderScope::Buffers => " file finder (buffers) ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

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

    let body_top = separator_row + 1;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return;
    }
    let body_width = inner.width;
    let show_preview = body_width >= 80;
    let (list_rect, preview_rect) = if show_preview {
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

    if let Some(preview_rect) = preview_rect {
        for row in list_rect.y..list_rect.y + list_rect.height {
            buf[(list_rect.x + list_rect.width, row)]
                .set_char('│')
                .set_style(muted_style);
        }
        render_preview(finder, preview_rect, theme, ws, buf);
    }

    render_list(finder, list_rect, theme, buf);
}

fn render_list(finder: &FileFinder, area: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let scroll = finder.selected.saturating_sub(rows.saturating_sub(1));
    let base = finder.base_paths();
    let end_x = area.x + area.width;
    let label_x = area.x + 1;

    for (row_idx, (&idx, indices)) in finder
        .filtered
        .iter()
        .zip(finder.match_indices.iter())
        .skip(scroll)
        .take(rows)
        .enumerate()
    {
        let row = area.y + row_idx as u16;
        let is_selected = scroll + row_idx == finder.selected;
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
