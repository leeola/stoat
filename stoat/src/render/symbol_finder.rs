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
    widgets::{Block, Borders, Clear, Widget},
};
use std::path::Path;

/// Lay out the centered document-symbol finder modal within `area`, or `None`
/// when `area` is too small to host it.
///
/// Returns the modal box, its inner rect (prompt, separator, then body), and
/// the list rect spanning the whole body below the separator. There is no
/// preview pane at this scope.
fn symbol_finder_layout(area: Rect) -> Option<(Rect, Rect, Rect)> {
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
    let list = Rect::new(inner.x, body_top, inner.width, body_height);
    Some((modal, inner, list))
}

pub(crate) fn render_symbol_finder(
    finder: &mut SymbolFinder,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let Some((modal, inner, list)) = symbol_finder_layout(area) else {
        return;
    };

    let title = match finder.scope {
        SymbolFinderScope::Document => " symbols (document) ",
        SymbolFinderScope::Workspace => " symbols (workspace) ",
    };
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    Clear.render(modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        modal,
        Some(title),
        modal_style,
        theme,
        scene.as_deref_mut(),
    );

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
        scene,
    );

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
