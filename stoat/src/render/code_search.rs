use crate::{
    code_search::{CodeSearchFinder, SearchMode},
    paths,
    render::text::{write_str, write_str_clipped},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_code_search(
    finder: &mut CodeSearchFinder,
    ws: &mut Workspace,
    theme: &Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    list_percent: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    // Search hits are unbounded and each one is worth reading in context, so the
    // modal asks for the whole area rather than measuring a list it would only
    // ever outgrow.
    let Some(layout) = crate::render::file_finder::file_finder_layout(
        area,
        (u16::MAX, u16::MAX),
        zoom,
        list_percent,
    ) else {
        return;
    };

    let git_root = ws.git_root.clone();
    let title = code_search_title(finder);
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    crate::render::clear_themed(layout.modal, buf, theme);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(&title),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let separator_style = theme.get(scope::UI_BORDER_INACTIVE);

    crate::render::picker::filter_header(
        buf,
        inner,
        ">",
        &finder.input,
        ws,
        theme,
        chrome,
        &mut *scene,
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
        crate::render::picker::render_picker_preview(
            &finder.preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
        );
    }

    finder.viewport_rows = Some(layout.list.height as usize);
    finder.preview_rows = layout.preview.map(|rect| rect.height as usize);
    let start_row =
        crate::render::picker::window_start(finder.selected, layout.list.height as usize);
    paint_match_rows(finder, layout.list, start_row, &git_root, theme, buf);
}

/// Paint the match list into `area`, starting at match `start_row`.
///
/// Each row shows a dim `path:line:col` location and the matched line's
/// snippet. The caller picks the window rather than the painter deriving it,
/// so a pooled page can paint rows the selection is nowhere near.
pub(crate) fn paint_match_rows(
    finder: &CodeSearchFinder,
    area: Rect,
    start_row: usize,
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
