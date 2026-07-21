use crate::{
    paths,
    render::text::{write_str, write_str_clipped},
    symbol_finder::{SymbolFinder, SymbolFinderScope, SymbolTarget},
    theme::{scope, Theme},
    workspace::Workspace,
};
use lsp_types::SymbolKind;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Widget},
};
use std::path::Path;
use stoat_language::LanguageRegistry;

/// Lay out the centered symbol finder modal within `area`, or `None` when
/// `area` is too small to host it.
///
/// Returns the modal box, its inner rect (prompt, separator, then body), the
/// result-list rect, and an optional preview pane rect. The preview appears only
/// when the body is wide enough, so a narrow modal stays list-only.
fn symbol_finder_layout(area: Rect) -> Option<(Rect, Rect, Rect, Option<Rect>)> {
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
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    let (list, preview) = crate::render::picker::split_list_preview(
        inner.x,
        body_top,
        inner.width,
        body_height,
        80,
        24,
    );
    Some((modal, inner, list, preview))
}

pub(crate) fn render_symbol_finder(
    finder: &mut SymbolFinder,
    ws: &mut Workspace,
    theme: &Theme,
    languages: &LanguageRegistry,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some((modal, inner, list, preview)) = symbol_finder_layout(area) else {
        return;
    };

    let title = match finder.scope {
        SymbolFinderScope::Document => " symbols (document) ",
        SymbolFinderScope::Workspace => " symbols (workspace) ",
    };
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    Clear.render(modal, buf);
    crate::render::chrome::modal_frame(buf, modal, Some(title), modal_style, theme, &mut *scene);

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

    if let Some(preview_rect) = preview {
        crate::render::chrome::vline(
            buf,
            list.x + list.width,
            list.y,
            list.height,
            separator_style,
            scene,
        );
        finder.preview_rows = Some(preview_rect.height as usize);
        let source_rect = match &finder.doc_markdown {
            Some(doc) => render_doc_pane(doc, preview_rect, separator_style, theme, languages, buf),
            None => preview_rect,
        };
        crate::render::picker::render_picker_preview(&finder.preview, source_rect, theme, ws, buf);
    }

    let git_root = ws.git_root.clone();
    finder.viewport_rows = Some(list.height as usize);
    paint_symbol_rows(finder, list, &git_root, theme, buf);
}

/// Paint the symbol list into `area`, following the selection so the selected
/// row stays visible. Each row shows the title with fuzzy-match highlighting on
/// the left and a dim kind and 1-based line suffix on the right.
fn paint_symbol_rows(
    finder: &SymbolFinder,
    area: Rect,
    git_root: &Path,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let start_row = finder.selected.saturating_sub(rows.saturating_sub(1));

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);
    let dim_style = theme.get(scope::UI_TEXT_MUTED);

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

        let entry = &finder.entries[idx];

        let suffix = match &entry.target {
            SymbolTarget::Workspace { path, .. } => {
                format!(
                    " {}:{}",
                    paths::display_relative(path, git_root),
                    entry.line + 1
                )
            },
            SymbolTarget::Offset(_) => {
                format!(" {} :{}", symbol_kind_label(entry.kind), entry.line + 1)
            },
        };
        let suffix_x = end_x.saturating_sub(suffix.chars().count() as u16);
        if suffix_x > label_x {
            let suffix_style = if is_selected { style } else { dim_style };
            write_str_clipped(buf, suffix_x, row, &suffix, suffix_style, end_x);
        }

        let title = &entry.title;
        let width = suffix_x.saturating_sub(label_x) as usize;
        let title_len = title.chars().count();
        let (dropped, text_x) = if title_len > width && width > 1 {
            let dropped = title_len - (width - 1);
            buf[(label_x, row)].set_char('\u{2026}').set_style(style);
            let tail_start = title
                .char_indices()
                .nth(dropped)
                .map_or(title.len(), |(byte, _)| byte);
            write_str_clipped(buf, label_x + 1, row, &title[tail_start..], style, suffix_x);
            (dropped, label_x + 1)
        } else {
            write_str_clipped(buf, label_x, row, title, style, suffix_x);
            (0, label_x)
        };

        for (title_col, _) in title.chars().enumerate().skip(dropped) {
            let col = text_x + (title_col - dropped) as u16;
            if col >= suffix_x {
                break;
            }
            if indices.binary_search(&(title_col as u32)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

/// Render the hover doc `markdown` into the top half of `area`, with an hline
/// below it, and return the rect the source preview should fill in the lower
/// rows. Lines beyond the pane width are clipped.
fn render_doc_pane(
    markdown: &str,
    area: Rect,
    separator_style: Style,
    theme: &Theme,
    languages: &LanguageRegistry,
    buf: &mut Buffer,
) -> Rect {
    let lines = crate::markdown::render_markdown(markdown, theme, languages);
    let max_doc_rows = (area.height / 2).max(1);
    let doc_rows = (lines.len() as u16).min(max_doc_rows);
    if doc_rows == 0 {
        return area;
    }

    let end_x = area.x + area.width;
    for (row_idx, line) in lines.iter().take(doc_rows as usize).enumerate() {
        let y = area.y + row_idx as u16;
        let mut x = area.x;
        for (text, style) in line {
            for ch in text.chars() {
                if x >= end_x {
                    break;
                }
                buf[(x, y)].set_char(ch).set_style(*style);
                x += 1;
            }
        }
    }

    let separator_row = area.y + doc_rows;
    crate::render::chrome::hline(
        buf,
        area.x,
        separator_row,
        area.width,
        separator_style,
        None,
    );
    let source_y = separator_row + 1;
    let source_height = (area.y + area.height).saturating_sub(source_y);
    Rect::new(area.x, source_y, area.width, source_height)
}

/// Short display label for a symbol's [`SymbolKind`], or empty when the server
/// gave none.
fn symbol_kind_label(kind: Option<SymbolKind>) -> &'static str {
    let Some(kind) = kind else {
        return "";
    };
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "mod",
        SymbolKind::NAMESPACE => "ns",
        SymbolKind::PACKAGE => "pkg",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "prop",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "ctor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "iface",
        SymbolKind::FUNCTION => "fn",
        SymbolKind::VARIABLE => "var",
        SymbolKind::CONSTANT => "const",
        SymbolKind::STRUCT => "struct",
        SymbolKind::ENUM_MEMBER => "variant",
        SymbolKind::TYPE_PARAMETER => "type",
        _ => "sym",
    }
}
