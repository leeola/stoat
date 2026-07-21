use crate::{
    code_search::{CodeSearchFinder, SearchMode},
    paths,
    render::text::{write_str, write_str_clipped},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};
use std::path::Path;

pub(crate) fn render_code_search(
    finder: &mut CodeSearchFinder,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some(layout) = crate::render::file_finder::file_finder_layout(area) else {
        return;
    };

    let git_root = ws.git_root.clone();
    let title = code_search_title(finder);
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(&title),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let prompt_style = theme.get(scope::UI_PROMPT);
    let separator_style = theme.get(scope::UI_BORDER_INACTIVE);

    write_str(buf, inner.x, inner.y, ">", prompt_style);
    let input_area = Rect::new(inner.x + 2, inner.y, inner.width.saturating_sub(2), 1);
    finder.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    crate::render::chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        separator_style,
        Some(&mut *scene),
    );

    if let Some(preview_rect) = layout.preview {
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            separator_style,
            scene,
        );
        crate::render::picker::render_picker_preview(&finder.preview, preview_rect, theme, ws, buf);
    }

    paint_match_rows(finder, layout.list, &git_root, theme, buf);
}

/// Paint the match list into `area`, following the selection so the selected row
/// stays visible. Each row shows a dim `path:line:col` location and the matched
/// line's snippet.
fn paint_match_rows(
    finder: &CodeSearchFinder,
    area: Rect,
    git_root: &Path,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let dim_style = theme.get(scope::UI_TEXT_MUTED);
    if finder.invalid_pattern {
        write_str(buf, area.x + 1, area.y, "invalid pattern", dim_style);
        return;
    }

    let start_row = finder.selected.saturating_sub(rows.saturating_sub(1));

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);

    let end_x = area.x + area.width;
    let label_x = area.x + 1;

    for (row_idx, m) in finder.matches.iter().skip(start_row).take(rows).enumerate() {
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

        let location = format!(
            "{}:{}:{}",
            paths::display_relative(&m.path, git_root),
            m.line,
            m.column,
        );
        let location_style = if is_selected { style } else { dim_style };
        write_str_clipped(buf, label_x, row, &location, location_style, end_x);

        let snippet_x = label_x + location.chars().count() as u16 + 2;
        if snippet_x < end_x {
            write_str_clipped(buf, snippet_x, row, &m.snippet, style, end_x);
        }
    }
}

/// Modal title carrying the active search mode, and for AST the target language.
fn code_search_title(finder: &CodeSearchFinder) -> String {
    match finder.mode {
        SearchMode::Regex => " code search: regex ".to_string(),
        SearchMode::Ast => {
            let lang = finder.target_lang.as_ref().map(|l| l.name).unwrap_or("?");
            format!(" code search: ast ({lang}) ")
        },
    }
}
