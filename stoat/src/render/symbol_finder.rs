use crate::{
    markdown::StyledLine,
    paths,
    render::text::write_str_clipped,
    symbol_finder::{SymbolFinder, SymbolFinderScope, SymbolTarget},
    theme::{scope, Theme},
    workspace::Workspace,
};
use lsp_types::SymbolKind;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};
use std::path::Path;
use stoat_language::LanguageRegistry;

/// Box rows the chrome takes around the list. Two borders, the prompt row, and
/// the separator under it.
const CHROME_ROWS: u16 = 4;

/// Lay out the centered symbol finder modal within `area`, or `None` when
/// `area` is too small to host it.
///
/// Returns the modal box, its inner rect (prompt, separator, then body), the
/// result-list rect, and an optional preview pane rect. The preview appears only
/// when the body is wide enough, so a narrow modal stays list-only.
///
/// `content_rows` is the symbols the list holds, which the box grows to fit past
/// its recommended 32 rows. `zoom` is the user's step count from
/// [`modal_zoom`](crate::app::Stoat::modal_zoom), and `list_percent` the width
/// the list takes from [`modal_split`](crate::app::Stoat::modal_split).
pub(crate) fn symbol_finder_layout(
    area: Rect,
    content_rows: u16,
    zoom: i8,
    list_percent: u16,
) -> Option<(Rect, Rect, Rect, Option<Rect>)> {
    let modal = crate::render::chrome::modal_box(
        area,
        (120, content_rows.saturating_add(CHROME_ROWS)),
        (120, 32),
        (40, 12),
        zoom,
    )?;
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
        crate::render::picker::MIN_PANE_COLUMNS,
        list_percent,
    );
    Some((modal, inner, list, preview))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_symbol_finder(
    finder: &mut SymbolFinder,
    ws: &mut Workspace,
    theme: &Theme,
    theme_epoch: u64,
    chrome: &crate::render::editor::ResolvedChrome,
    languages: &LanguageRegistry,
    area: Rect,
    zoom: i8,
    list_percent: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some((modal, inner, list, preview)) =
        symbol_finder_layout(area, finder.content_rows, zoom, list_percent)
    else {
        return;
    };

    let title = match finder.scope {
        SymbolFinderScope::Document => " symbols (document) ",
        SymbolFinderScope::Workspace => " symbols (workspace) ",
    };
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    crate::render::clear_themed(modal, buf, theme);
    crate::render::chrome::modal_frame(buf, modal, Some(title), modal_style, theme, &mut *scene);

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
        let source_rect = match finder.styled_doc_lines(theme_epoch, theme, languages) {
            Some(lines) => render_doc_pane(lines, preview_rect, separator_style, buf, scene),
            None => preview_rect,
        };
        crate::render::picker::render_picker_preview(
            &finder.preview,
            source_rect,
            theme,
            chrome,
            ws,
            buf,
        );
    }

    let git_root = ws.git_root.clone();
    finder.viewport_rows = Some(list.height as usize);
    let start_row = crate::render::picker::window_start(finder.selected, list.height as usize);
    paint_symbol_rows(finder, list, start_row, &git_root, theme, buf);
}

/// Paint the symbol list into `area`, starting at symbol `start_row`.
///
/// Each row shows the title with fuzzy-match highlighting on the left and a dim
/// kind and 1-based line suffix on the right. The caller picks the window
/// rather than the painter deriving it, so a pooled page can paint rows the
/// selection is nowhere near.
pub(crate) fn paint_symbol_rows(
    finder: &SymbolFinder,
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

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);
    let dim_style = theme.get(scope::UI_TEXT_MUTED);

    let end_x = area.x + area.width;
    let label_x = area.x + 1;

    // Reused across the painted rows rather than allocated per row, since a row
    // past the indexed block derives its offsets here.
    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    for (row_idx, &idx) in finder
        .filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let indices = finder.row_indices(start_row + row_idx, &mut derived, &mut matching);
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

/// Paint the hover doc `lines` into the top half of `area`, with an hline below
/// them, and return the rect the source preview should fill in the lower rows.
/// Lines beyond the pane width are clipped.
///
/// Takes the lines already styled rather than the markdown behind them. This
/// runs on every paint the modal is up for, and rendering markdown parses each
/// fenced code block and builds a style per byte.
fn render_doc_pane(
    lines: &[StyledLine],
    area: Rect,
    separator_style: Style,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> Rect {
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
        scene,
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

#[cfg(test)]
mod tests {
    use super::{render_doc_pane, symbol_finder_layout};
    use crate::render::picker::DEFAULT_LIST_PERCENT;
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

    /// The doc pane used to pass no scene, so its separator fell back to dash
    /// glyphs while every sibling separator in the same modal drew a hairline.
    /// A hairline writes no glyphs at all, so the absence of dashes is what
    /// says the scene arrived.
    #[test]
    fn the_doc_separator_is_a_hairline_when_the_style_resolves() {
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let rgb = Style::default().fg(ratatui::style::Color::Rgb(1, 2, 3));

        let doc = vec![vec![("one line of doc".to_string(), Style::default())]];
        let source = render_doc_pane(&doc, area, rgb, &mut buf, &mut scene);

        let separator_row = source.y - 1;
        let painted: String = (area.x..area.x + area.width)
            .map(|x| buf[(x, separator_row)].symbol())
            .collect();
        assert!(
            !painted.contains('\u{2500}'),
            "the separator row holds no dash glyphs: {painted:?}"
        );
        assert!(
            !scene.buffer().is_empty(),
            "and the hairline reached the scene"
        );
    }

    #[test]
    fn a_short_symbol_list_keeps_the_recommended_box() {
        let (modal, ..) =
            symbol_finder_layout(Rect::new(0, 0, 200, 60), 5, 0, DEFAULT_LIST_PERCENT)
                .expect("the area hosts the finder");
        assert_eq!(
            modal,
            Rect::new(40, 14, 120, 32),
            "content under the recommended size leaves the box at 120x32, centered"
        );
    }

    /// The box has to carry exactly the chrome its list sits under, or a list
    /// sized to fit would still scroll by however far the count is off.
    #[test]
    fn a_long_symbol_list_grows_the_box_to_show_every_row() {
        let (modal, _, list, _) =
            symbol_finder_layout(Rect::new(0, 0, 200, 60), 40, 0, DEFAULT_LIST_PERCENT)
                .expect("the area hosts the finder");
        assert_eq!(modal.height, 44, "forty symbols plus four chrome rows");
        assert_eq!(
            list.height, 40,
            "and the body then holds all forty without scrolling"
        );
    }

    #[test]
    fn a_list_larger_than_the_area_stops_at_the_margin() {
        let (modal, ..) =
            symbol_finder_layout(Rect::new(0, 0, 200, 60), u16::MAX, 0, DEFAULT_LIST_PERCENT)
                .expect("the area hosts the finder");
        assert_eq!(modal.height, 56, "growth stops at the area less its margin");
    }
}
