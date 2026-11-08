//! Shared painters for a [`crate::picker::PathPicker`]'s result list and
//! preview.
//!
//! The standalone file finder and the palette's inline argument picker paint
//! the same list of repo-relative rows beside the same preview pane. Both drive
//! a `PathPicker`, so both render through these functions and cannot drift.

use crate::{
    fuzzy,
    input_view::InputView,
    picker::{write_row_display, PickList, Preview},
    render::{
        chrome::TypeTag,
        editor::{render_editor, ResolvedChrome},
        paint::style_rgb,
        text::{write_str, write_str_clipped},
    },
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use stoat_widgets::bar::Bar;
use unicode_width::UnicodeWidthStr;

/// The glyph before a filter modal's query. The command palette keeps `:` in
/// its place, the command-line prompt that its commands follow.
pub(crate) const FILTER_PROMPT: &str = "\u{203A}";

/// Columns the query keeps on a filter modal's header row. A row too narrow
/// for the query and the words right of it drops the chip first, then the
/// count.
const MIN_QUERY_COLUMNS: u16 = 12;

/// Page dots a modal footer draws at most. A longer list maps its pages onto
/// these, so the dots keep clear of the hints however long the list grows.
const MAX_PAGE_DOTS: usize = 5;

/// The page dots' geometry in sixteenths of a cell: the current page's width,
/// any other page's width, the gap between two dots, a dot's height, and its
/// top edge in the row.
const CURRENT_DOT_WIDTH: u16 = 24;
const PAGE_DOT_WIDTH: u16 = 6;
const PAGE_DOT_GAP: u16 = 8;
const PAGE_DOT_HEIGHT: u16 = 4;
const PAGE_DOT_TOP: i16 = 6;

/// The words a filter modal's header row shows around its query.
pub(crate) struct FilterHeader<'a> {
    /// The modal's name, painted first.
    pub(crate) label: &'a str,
    /// The glyph before the query, [`FILTER_PROMPT`] in all but the palette.
    pub(crate) prompt: &'a str,
    /// How many entries the query keeps, and how many the list holds.
    pub(crate) count: Option<(usize, usize)>,
    /// The notes at the row's right end, left to right, each with whether it
    /// is the active one.
    ///
    /// A scope switch names every scope and marks the one the list shows. A
    /// lone active note names the key that switches the scope.
    pub(crate) chips: &'a [(&'a str, bool)],
}

/// Paint a filter modal's header, which is the label, the prompt glyph, the
/// live input, the count and chips right of it, and the separator ruling the
/// row off from the body.
///
/// `inner` is the modal's rect inside its border. The header claims its first
/// two rows, so a caller's body starts at `inner.y + 2`.
///
/// The input starts one column past the prompt, so a longer label or prompt
/// pushes the input right.
///
/// Every filter modal opens this way, and each lays its body rects out from
/// that same two-row assumption. One painter is what keeps the assumption true
/// across all of them at once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_header(
    buf: &mut Buffer,
    inner: Rect,
    header: &FilterHeader<'_>,
    input: &InputView,
    ws: &mut Workspace,
    theme: &Theme,
    chrome: &ResolvedChrome,
    scene: &mut stoat_widgets::ApcScene,
) {
    let input_area = paint_header_row(buf, inner, header, theme, scene);
    input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        chrome,
        &BTreeMap::new(),
        buf,
    );

    crate::render::chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        theme.get(scope::UI_BORDER_INACTIVE),
        scene,
    );
}

/// Paint the words of the header row into `inner`'s first row, and return the
/// rect left for the query.
///
/// The count and the chips sit flush with the row's right end, one column
/// apart, and one column clear of the query. An active chip's box goes through
/// [`chip_cells`](crate::render::chrome::chip_cells), so a live scene draws it
/// as a panel. An inactive chip is plain muted text on the row's background. A
/// row too narrow for all of them drops the chips together, then the count.
///
/// The count reads muted. A count of no shown entries paints its zero in
/// `diff.deleted`'s foreground, which flags a query that matches nothing.
fn paint_header_row(
    buf: &mut Buffer,
    inner: Rect,
    header: &FilterHeader<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) -> Rect {
    let (y, end) = (inner.y, inner.x + inner.width);

    let mut x = inner.x;
    if !header.label.is_empty() {
        let style = theme.get(scope::UI_TEXT_DIM).add_modifier(Modifier::BOLD);
        write_str_clipped(buf, x, y, header.label, style, end);
        x = x.saturating_add(header.label.width() as u16 + 1);
    }
    write_str_clipped(buf, x, y, header.prompt, theme.get(scope::UI_PROMPT), end);
    let query_x = x.saturating_add(header.prompt.width() as u16 + 1).min(end);

    let mut count = header
        .count
        .map(|(shown, total)| format!("{shown} of {total}"));
    let mut chips: Vec<(String, bool)> = header
        .chips
        .iter()
        .map(|&(chip, active)| (format!(" {chip} "), active))
        .collect();
    let fits = |count: &Option<String>, chips: &[(String, bool)]| {
        let trailing: u16 = count
            .iter()
            .chain(chips.iter().map(|(chip, _)| chip))
            .map(|part| part.width() as u16 + 1)
            .sum();
        query_x + MIN_QUERY_COLUMNS + trailing <= end
    };
    if !fits(&count, &chips) {
        chips.clear();
    }
    if !fits(&count, &chips) {
        count = None;
    }

    let mut right = end;
    for (chip, active) in chips.iter().rev() {
        let width = chip.width() as u16;
        right -= width;
        let style = match active {
            true => {
                let cells = Rect::new(right, y, width, 1);
                crate::render::chrome::chip_cells(cells, theme.get(scope::UI_CHIP), scene)
            },
            false => theme.get(scope::UI_TEXT_MUTED),
        };
        write_str(buf, right, y, chip, style);
        right -= 1;
    }
    if let Some(count) = count {
        right -= count.width() as u16;
        let muted = theme.get(scope::UI_TEXT_MUTED);
        write_str(buf, right, y, &count, muted);
        if let Some((0, _)) = header.count {
            let zero = match theme.get(scope::DIFF_DELETED).fg {
                Some(fg) => muted.fg(fg),
                None => muted,
            };
            write_str(buf, right, y, "0", zero);
        }
        right -= 1;
    }
    Rect::new(query_x, y, right.saturating_sub(query_x), 1)
}

/// What a modal's footer row shows, from its left edge to its right.
#[derive(Default)]
pub(crate) struct Footer<'a> {
    /// Styled spans painted first, such as a legend of the row marks.
    pub(crate) lead: &'a [(&'a str, Style)],
    /// Key hints, each a bound key and the word for what it does.
    pub(crate) hints: &'a [(&'a str, &'a str)],
    /// A note set near the right edge, such as the context a modal opened in.
    pub(crate) note: Option<&'a str>,
    /// The selection's page and the page count, as [`page_of`] gives them.
    pub(crate) pages: Option<(usize, usize)>,
}

/// Paint a modal's footer row into `rect`, with its lead and key hints from the
/// left, a note right of them, and its page dots at the right.
///
/// The lead's spans paint in their own styles, two cells before the hints. Each
/// hint is a key in `ui.key_label` and the word it does in `ui.text.muted`. The
/// first hint that does not fit, and every hint after it, is left out. The
/// note ends one cell from the dots or the right edge, in `ui.text.muted`, and
/// is left out when it does not clear the hints. The page dots draw only under
/// a live scene.
pub(crate) fn modal_footer(
    buf: &mut Buffer,
    rect: Rect,
    footer: &Footer<'_>,
    theme: &Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let style = theme.get(scope::UI_MODAL_FOOTER);
    let row_end = rect.x + rect.width;
    for x in rect.x..row_end {
        buf[(x, rect.y)].set_char(' ').set_style(style);
    }

    let hints_end = paint_page_dots(rect, footer.pages, theme, buf, scene);
    let mut x = rect.x + 1;
    for (text, span_style) in footer.lead {
        write_str_clipped(buf, x, rect.y, text, *span_style, row_end);
        x = x.saturating_add(text.width() as u16);
    }
    if !footer.lead.is_empty() {
        x = x.saturating_add(2);
    }

    let key_style = theme.get(scope::UI_KEY_LABEL);
    let word_style = theme.get(scope::UI_TEXT_MUTED);
    for (key, word) in footer.hints {
        let width = (key.width() + 1 + word.width()) as u16;
        if x + width > hints_end {
            break;
        }
        write_str(buf, x, rect.y, key, key_style);
        write_str(buf, x + key.width() as u16 + 1, rect.y, word, word_style);
        x += width + 2;
    }

    let note_x = footer.note.and_then(|note| {
        let x_end = hints_end.saturating_sub(1);
        Some((note, x_end.checked_sub(note.width() as u16)?))
    });
    if let Some((note, note_x)) = note_x.filter(|&(_, note_x)| note_x >= x) {
        write_str(buf, note_x, rect.y, note, word_style);
    }
}

/// The color of a strong line on an overlay, which is the `ui.surface.overlay`
/// stroke, or `ui.border.inactive` in a theme that sets no overlay stroke.
pub(crate) fn line_strong(theme: &Theme) -> Option<[u8; 3]> {
    theme
        .try_get(scope::UI_SURFACE_OVERLAY)
        .and_then(|style| style_rgb(style.fg))
        .or_else(|| style_rgb(theme.get(scope::UI_BORDER_INACTIVE).fg))
}

/// Draw the page dots of `pages` at the right of `rect`, and return the column
/// the dots start at, which is `rect`'s right edge when none draw.
///
/// The dots end one cell from the right edge. Past [`MAX_PAGE_DOTS`] pages the
/// lit dot is the one at the selection's share of the list.
fn paint_page_dots(
    rect: Rect,
    pages: Option<(usize, usize)>,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    let end = rect.x + rect.width;
    let Some((current, count)) = pages.filter(|&(_, count)| count > 0 && scene.live()) else {
        return end;
    };
    let lit_color = style_rgb(theme.get(scope::UI_BORDER_FOCUSED).fg);
    let (Some(lit_color), Some(dim_color)) = (lit_color, line_strong(theme)) else {
        return end;
    };

    let dots = count.min(MAX_PAGE_DOTS);
    let lit = current.min(count - 1) * dots / count;
    let total = CURRENT_DOT_WIDTH + (dots as u16 - 1) * (PAGE_DOT_WIDTH + PAGE_DOT_GAP);
    let Some(start) = (rect.width.saturating_sub(1) * 16).checked_sub(total) else {
        return end;
    };

    let origin = Rect::new(rect.x, rect.y, 1, 1);
    let mut x = start;
    for dot in 0..dots {
        let (width, color) = match dot == lit {
            true => (CURRENT_DOT_WIDTH, lit_color),
            false => (PAGE_DOT_WIDTH, dim_color),
        };
        Bar {
            x: x as i16,
            y: PAGE_DOT_TOP,
            width,
            height: PAGE_DOT_HEIGHT,
            color,
        }
        .render(origin, buf, scene);
        x += width + PAGE_DOT_GAP;
    }
    rect.x + start / 16
}

/// The page the selection is on and the pages a list of `len` entries spans
/// at `rows` a page, or `None` for an empty list or a list with no rows on
/// screen.
pub(crate) fn page_of(selected: usize, len: usize, rows: usize) -> Option<(usize, usize)> {
    if len == 0 || rows == 0 {
        return None;
    }
    Some((selected.min(len - 1) / rows, len.div_ceil(rows)))
}

/// First entry visible when `selected` is showing, given `rows` on screen.
///
/// The window is anchored to the bottom. A selection past it rides the last
/// visible row, and one still above it leaves the list where it is. Every
/// scrolling modal list follows this, so a picker's hit test and its paint
/// agree on which entry a screen row holds.
///
/// A list shorter than its window starts at zero, which is what makes this
/// safe to call before knowing whether scrolling is needed at all.
pub(crate) fn window_start(selected: usize, rows: usize) -> usize {
    selected.saturating_sub(rows.saturating_sub(1))
}

/// Rows of chrome the box spends before its list. The border takes two, the
/// filter prompt one, and the rule under it one more.
const CHROME_ROWS: u16 = 4;

/// Where a target picker's parts sit inside `area`.
///
/// One shape for every jump-target list, so they open the same size and split
/// their body the same way.
pub(crate) struct PickerLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border, starting at the filter prompt.
    pub(crate) inner: Rect,
    /// The candidate rows.
    pub(crate) list: Rect,
    /// The selected candidate's file, when the box is wide enough to show one.
    pub(crate) preview: Option<Rect>,
}

/// Lay a target picker out within `area`, or [`None`] when `area` is too small
/// to host it.
///
/// Painting and hit-testing both go through this, so a clicked row names the
/// row drawn there.
pub(crate) fn target_picker_layout(
    area: Rect,
    zoom: i8,
    list_percent: u16,
) -> Option<PickerLayout> {
    let modal = crate::render::chrome::modal_box(
        area,
        (120, 32u16.saturating_add(CHROME_ROWS)),
        (crate::render::chrome::list_modal_width(area), 32),
        (40, 12),
        zoom,
    )?;
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    let (list, preview) = split_list_preview(
        inner.x,
        body_top,
        inner.width,
        body_height,
        80,
        MIN_PANE_COLUMNS,
        list_percent,
    );
    Some(PickerLayout {
        modal,
        inner,
        list,
        preview,
    })
}

/// Fixtures for asserting which screen row a picker paints its selection on.
///
/// The theme and the buffer scan only mean anything together. The theme colors
/// `ui.selection` and nothing else, so a scan for that background finds the
/// selected row and can find no other, and a picker that scrolls its selection
/// off-screen highlights nothing at all.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::theme::Theme;
    use ratatui::{buffer::Buffer, style::Color};

    /// The background [`selected_rows`] scans for.
    pub(crate) const SELECTION_BG: Color = Color::Rgb(255, 0, 0);

    pub(crate) fn selection_theme() -> Theme {
        let src = r##"theme t { ui.selection.bg = "#ff0000"; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The foreground [`match_theme`] gives a fuzzy-match character.
    pub(crate) const MATCH_FG: Color = Color::Rgb(255, 0, 0);

    pub(crate) fn match_theme() -> Theme {
        let src = r##"theme t { ui.search.match.fg = "#ff0000"; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// Rows of `buf` carrying [`SELECTION_BG`], in ascending order.
    pub(crate) fn selected_rows(buf: &Buffer) -> Vec<u16> {
        let area = buf.area;
        (area.y..area.y + area.height)
            .filter(|&row| {
                (area.x..area.x + area.width).any(|col| buf[(col, row)].bg == SELECTION_BG)
            })
            .collect()
    }

    pub(crate) fn row_text(buf: &Buffer, row: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }
}

/// Paint a picker's result rows into `area`, one repo-relative path per row,
/// with the selected row and fuzzy-match characters highlighted.
///
/// `start_row` is the first filtered index to show, so the live list can derive
/// it from the selection while the smooth-scroll pool paints absolute pages, and
/// both render identical rows. Rows are read from `picklist.base`, which every
/// caller keeps in sync with its display set on refilter.
///
/// `row_style` paints every row but the selected one, which takes
/// [`Theme::list_selected`]. A caller passes its own style to set a whole
/// list apart, such as a list of ignored files. The selected row's accent bar
/// goes into `scene`.
///
/// A path row keeps columns 1 and 2 for its [`chrome::type_tag`], so tagged and
/// untagged rows align, and a wider tag pushes only its own row's label right.
/// Its directory paints in `ui.text.muted` and its name in the row's style. A
/// path found in `marks`, which is sorted, ends in an `M` in the
/// `diff.modified` color. A relative entry, such as a branch name, gets none
/// of these.
///
/// [`chrome::type_tag`]: crate::render::chrome::type_tag
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_path_rows(
    picklist: &PickList,
    git_root: &Path,
    home: Option<&Path>,
    prefix: &str,
    marks: &[PathBuf],
    area: Rect,
    start_row: usize,
    theme: &Theme,
    row_style: Style,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let selected_style = theme.list_selected(scope::UI_SELECTION);
    let match_style = theme.list_match();
    let dir_style = theme.get(scope::UI_TEXT_MUTED);
    let mark_style = theme.get(scope::DIFF_MODIFIED);

    let area_end = area.x + area.width;
    let prefix_len = prefix.chars().count() as u32;
    let mut label = String::new();

    // Rows past the eagerly indexed block derive their offsets here, one row at
    // a time into buffers the whole window shares.
    let mut derived = Vec::new();
    let mut matching = fuzzy::Scratch::default();

    for (row_idx, &idx) in picklist
        .filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let indices = picklist.row_indices(start_row + row_idx, &mut derived, &mut matching);
        let row = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == picklist.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..area_end {
            buf[(col, row)].set_char(' ').set_style(style);
        }
        if is_selected {
            crate::render::chrome::selection_bar(area.x, row, theme, buf, scene);
        }

        let path = &picklist.base[idx];
        let is_path = path.is_absolute();
        let label_x = match is_path {
            true => paint_type_tag(path, area.x + 1, row, style, theme, buf, area_end),
            false => area.x + 1,
        };
        let marked = is_path && area_end >= label_x + 3 && marks.binary_search(path).is_ok();
        let end_x = match marked {
            true => {
                buf[(area_end - 1, row)].set_char('M').set_style(mark_style);
                area_end - 2
            },
            false => area_end,
        };

        label.clear();
        label.push_str(prefix);
        write_row_display(
            &mut label,
            path,
            git_root,
            picklist.display_roots.as_deref(),
            home,
        );
        let name_col = if is_path { name_start(&label) } else { 0 };
        let width = end_x.saturating_sub(label_x) as usize;
        let label_len = label.chars().count();

        // A row wider than its column gives up chars to an ellipsis in one cell,
        // leaving `width - 1` cells for the rest. A path is start-truncated
        // helix-style so the file name at the tail stays on screen. A relative
        // entry is a label rather than a path, such as a branch name of the
        // palette's argument list, and its head names it, so its tail goes
        // instead.
        let (dropped, text_x, text_end) = if label_len > width && width > 1 {
            if is_path {
                let dropped = label_len - (width - 1);
                buf[(label_x, row)].set_char('\u{2026}').set_style(style);
                let tail_start = label
                    .char_indices()
                    .nth(dropped)
                    .map_or(label.len(), |(byte, _)| byte);
                write_str_clipped(buf, label_x + 1, row, &label[tail_start..], style, end_x);
                (dropped, label_x + 1, end_x)
            } else {
                let ellipsis_x = end_x - 1;
                write_str_clipped(buf, label_x, row, &label, style, ellipsis_x);
                buf[(ellipsis_x, row)].set_char('\u{2026}').set_style(style);
                (0, label_x, ellipsis_x)
            }
        } else {
            write_str_clipped(buf, label_x, row, &label, style, end_x);
            (0, label_x, end_x)
        };

        for (label_col, _) in label.chars().enumerate().skip(dropped) {
            let col = text_x + (label_col - dropped) as u16;
            if col >= text_end {
                break;
            }
            if label_col < name_col {
                buf[(col, row)].set_style(dir_style);
            }
            let label_col = label_col as u32;
            // The literal `prefix` is never matched. The picklist's indices are
            // offsets into the part after it, so shift them past the prefix.
            if label_col >= prefix_len && indices.binary_search(&(label_col - prefix_len)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

/// Columns a path row keeps for its type tag, which fit a two-letter tag such
/// as `rs`.
const TAG_COLUMNS: u16 = 2;

/// Paint `path`'s type tag from column `x` of `row`, and return the column its
/// label starts at.
///
/// The label starts one blank column past [`TAG_COLUMNS`], or past a tag wider
/// than them.
fn paint_type_tag(
    path: &Path,
    x: u16,
    row: u16,
    style: Style,
    theme: &Theme,
    buf: &mut Buffer,
    end_x: u16,
) -> u16 {
    let width = match crate::render::chrome::type_tag(path) {
        Some(TypeTag {
            tag,
            scope: tag_scope,
            ..
        }) => {
            write_str_clipped(buf, x, row, tag, style.patch(theme.get(tag_scope)), end_x);
            tag.width() as u16
        },
        None => 0,
    };
    x + width.max(TAG_COLUMNS) + 1
}

/// The char index where `label`'s last path component starts, which is the
/// name a row or the preview header paints apart from its directory.
pub(crate) fn name_start(label: &str) -> usize {
    match label.rfind('/') {
        Some(slash) => label[..=slash].chars().count(),
        None => 0,
    }
}

/// Paint the picker's preview pane by rendering its scratch editor into `area`.
/// A no-op for an empty rect.
pub(crate) fn render_picker_preview(
    preview: &Preview,
    area: Rect,
    theme: &Theme,
    chrome: &ResolvedChrome,
    ws: &mut Workspace,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fallback = theme.get(scope::UI_TEXT);
    if let Some(editor) = ws.editors.get_mut(preview.editor) {
        render_editor(editor, area, fallback, theme, Some(chrome), buf, false);
    }
}

/// Share of a modal's body its list pane takes, before the user drags the
/// separator to a share of their own.
///
/// One value for the whole modal family, so an untouched finder, symbol list,
/// code search, and commit picker all divide their body the same way.
pub(crate) const DEFAULT_LIST_PERCENT: u16 = 40;

/// Columns each pane of a side-by-side modal keeps.
///
/// A list narrower than this cannot show a path worth reading, and a preview
/// narrower cannot show code, so the split refuses to squeeze either below it
/// however the share is set or dragged.
pub(crate) const MIN_PANE_COLUMNS: u16 = 24;

/// Split a picker body rect into a result list and an optional preview pane.
///
/// The preview appears only when `width >= wide_threshold`, where the list
/// takes `list_percent` of the width (floored at `min_list`) and the preview the
/// rest past a one-cell separator. Below the threshold the list takes the full
/// width and there is no preview.
pub(crate) fn split_list_preview(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    wide_threshold: u16,
    min_list: u16,
    list_percent: u16,
) -> (Rect, Option<Rect>) {
    if width >= wide_threshold {
        let list_width = (width * list_percent / 100).max(min_list);
        let preview_width = width.saturating_sub(list_width + 1);
        (
            Rect::new(x, y, list_width, height),
            Some(Rect::new(x + list_width + 1, y, preview_width, height)),
        )
    } else {
        (Rect::new(x, y, width, height), None)
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::match_theme, *};
    use crate::picker::INDEXED_ROWS;
    use ratatui::style::Color;
    use std::path::PathBuf;
    use stoatty_protocol::command::{
        decode_stream, encode_bar, BarCommand, BorderStyle, Command, PanelCommand, PanelShadow,
    };

    /// The header a finder-like modal paints, its chip at the row's end.
    const FINDER_HEADER: FilterHeader<'static> = FilterHeader {
        label: "Files",
        prompt: FILTER_PROMPT,
        count: Some((14, 890)),
        chips: &[("S-Tab \u{00B7} git only", true)],
    };

    /// The text of a finder-like header painted `width` columns wide, and the
    /// rect it leaves the query.
    fn header_row(width: u16) -> (String, Rect) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let query = paint_header_row(&mut buf, area, &FINDER_HEADER, &Theme::empty(), &mut scene);
        (test_support::row_text(&buf, 0), query)
    }

    /// Under a live scene the chip's box is a chip panel over its 18 cells, and
    /// the cells take the chip's text color over the row's own background.
    #[test]
    fn a_live_header_draws_its_chip_box_as_a_panel() {
        let theme = {
            let (config, _) =
                stoat_config::parse(r##"theme t { ui.chip = { fg: "#010203", bg: "#040506" }; }"##);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &FINDER_HEADER, &theme, &mut scene);

        let cells: Vec<_> = (42..60).map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg)).collect();
        assert_eq!(
            (cells, decode_stream(scene.bytes())),
            (
                vec![(Color::Rgb(1, 2, 3), Color::Reset); 18],
                vec![Command::Panel(PanelCommand {
                    top: 0,
                    left: 42,
                    width: 18,
                    height: 1,
                    style: BorderStyle::None_,
                    border: [4, 5, 6],
                    corner_radius: 1,
                    fill: Some([4, 5, 6]),
                    shadow: PanelShadow::None_,
                    inset_x: 0,
                    inset_y: 1,
                    corners: 0,
                    fillets: 0,
                    hatch: None,
                    above_pools: false,
                    backdrop: false,
                    anchor: None,
                })],
            ),
        );
    }

    #[test]
    fn the_header_sets_its_count_and_chip_flush_right() {
        assert_eq!(
            header_row(60),
            (
                format!(
                    "Files \u{203A} {}14 of 890  S-Tab \u{00B7} git only ",
                    " ".repeat(24)
                ),
                Rect::new(8, 0, 23, 1),
            ),
            "the chip ends the row, and the count stands one column left of it",
        );
    }

    /// A scope header names every scope, right to left and one column apart.
    /// The active one is a chip panel over its cells, and the other is plain
    /// muted text with no box.
    #[test]
    fn a_scope_header_boxes_the_active_chip_and_leaves_the_other_plain() {
        let theme = {
            let src = r##"theme t {
                ui.chip = { fg: "#010203", bg: "#040506" };
                ui.text.muted.fg = "#070809";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = FilterHeader {
            label: "help",
            prompt: FILTER_PROMPT,
            count: Some((14, 214)),
            chips: &[("applicable", true), ("all", false)],
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &header, &theme, &mut scene);

        let panels: Vec<_> = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((panel.left, panel.width)),
                _ => None,
            })
            .collect();
        assert_eq!(
            (
                test_support::row_text(&buf, 0)
                    .chars()
                    .skip(32)
                    .collect::<String>(),
                panels,
                [43, 56].map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg)),
            ),
            (
                "14 of 214  applicable   all ".to_owned(),
                vec![(42, 12)],
                [
                    (Color::Rgb(1, 2, 3), Color::Reset),
                    (Color::Rgb(7, 8, 9), Color::Reset)
                ],
            ),
        );
    }

    /// A count of no shown entries reads its zero in `diff.deleted`'s
    /// foreground over the muted background, and the rest of the count muted.
    #[test]
    fn a_zero_count_reads_its_zero_in_the_deleted_color() {
        let theme = {
            let src = r##"theme t {
                diff.deleted = { fg: "#010203", bg: "#040506" };
                ui.text.muted = { fg: "#070809", bg: "#0a0b0c" };
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let header = FilterHeader {
            label: "Files",
            prompt: FILTER_PROMPT,
            count: Some((0, 214)),
            chips: &[],
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_header_row(&mut buf, area, &header, &theme, &mut scene);

        let muted = (Color::Rgb(7, 8, 9), Color::Rgb(10, 11, 12));
        assert_eq!(
            (
                test_support::row_text(&buf, 0),
                (32..40)
                    .map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg))
                    .collect::<Vec<_>>(),
            ),
            (
                format!("Files \u{203A} {}0 of 214", " ".repeat(24)),
                [
                    vec![(Color::Rgb(1, 2, 3), Color::Rgb(10, 11, 12))],
                    vec![muted; 7],
                ]
                .concat(),
            ),
        );
    }

    /// The query's width tells which words stayed. At 49 columns both fit with
    /// the query at its floor, at 48 only the count does, at 30 the count fits
    /// at the floor again, and at 29 neither does.
    #[test]
    fn a_narrow_header_drops_its_chip_and_then_its_count() {
        assert_eq!(
            [49, 48, 30, 29].map(|width| header_row(width).1.width),
            [12, 30, 12, 21],
            "the chip goes first, then the count, so the query keeps its floor",
        );
    }

    #[test]
    fn page_of_counts_pages_of_whole_rows() {
        assert_eq!(
            [
                page_of(0, 30, 10),
                page_of(29, 30, 10),
                page_of(10, 11, 10),
                page_of(5, 0, 10),
                page_of(5, 9, 0),
            ],
            [Some((0, 3)), Some((2, 3)), Some((1, 2)), None, None],
        );
    }

    /// The text of a footer painted `width` columns wide in a live scene, and
    /// the bar frames its dots send.
    fn footer_row(
        width: u16,
        note: Option<&str>,
        pages: Option<(usize, usize)>,
    ) -> (String, Vec<u8>) {
        let src = r##"theme t {
            ui.border.focused.fg = "#010203";
            ui.surface.overlay.fg = "#040506";
        }"##;
        let (config, _) = stoat_config::parse(src);
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");

        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let hints = [("Enter", "open"), ("C-f/C-b", "page")];
        let footer = Footer {
            hints: &hints,
            note,
            pages,
            ..Footer::default()
        };
        modal_footer(&mut buf, area, &footer, &theme, &mut scene);
        (test_support::row_text(&buf, 0), scene.buffer().clone())
    }

    /// The bar frames of page dots at `(x, width, color)`, in sixteenths.
    fn dot_frames(dots: &[(i16, u16, [u8; 3])]) -> Vec<u8> {
        dots.iter()
            .flat_map(|&(x, width, color)| {
                encode_bar(&BarCommand {
                    x,
                    y: 6,
                    width,
                    height: 4,
                    color,
                    occludes: false,
                })
            })
            .collect()
    }

    /// The dots end one cell from the right edge, the lit one wide, and the
    /// hints stop short of them. Past five pages the lit dot stands at the
    /// selection's share of the list.
    #[test]
    fn the_footer_paints_its_hints_and_page_dots() {
        let (lit, dim) = ([1, 2, 3], [4, 5, 6]);
        assert_eq!(
            [
                footer_row(40, None, Some((1, 3))),
                footer_row(20, None, Some((0, 2))),
                footer_row(40, None, Some((3, 7)))
            ],
            [
                (
                    format!(" Enter open  C-f/C-b page{}", " ".repeat(15)),
                    dot_frames(&[(572, 6, dim), (586, 24, lit), (618, 6, dim)]),
                ),
                (
                    format!(" Enter open{}", " ".repeat(9)),
                    dot_frames(&[(266, 24, lit), (298, 6, dim)]),
                ),
                (
                    format!(" Enter open  C-f/C-b page{}", " ".repeat(15)),
                    dot_frames(&[
                        (544, 6, dim),
                        (558, 6, dim),
                        (572, 24, lit),
                        (604, 6, dim),
                        (618, 6, dim),
                    ]),
                ),
            ],
        );
    }

    /// A note ends one cell from the right edge. A note that does not clear the
    /// hints by two cells is left out.
    #[test]
    fn the_footer_sets_its_note_right_of_the_hints() {
        assert_eq!(
            [
                footer_row(40, Some("normal"), None),
                footer_row(30, Some("normal"), None),
            ],
            [
                (
                    format!(" Enter open  C-f/C-b page{}normal ", " ".repeat(8)),
                    Vec::new(),
                ),
                (
                    format!(" Enter open  C-f/C-b page{}", " ".repeat(5)),
                    Vec::new(),
                ),
            ],
        );
    }

    #[test]
    fn the_window_holds_the_selection_on_the_last_row() {
        assert_eq!(window_start(3, 12), 0, "a selection on screen scrolls not");
        assert_eq!(window_start(14, 12), 3, "one past it scrolls just enough");
        assert_eq!(
            window_start(0, 0),
            0,
            "and a list with no room to paint has nowhere to scroll to"
        );
    }

    /// The share is what the drag-resizable separator writes, so the layouts it
    /// feeds have to move with it, and an untouched modal has to land exactly
    /// where it landed before the share was a parameter at all.
    #[test]
    fn the_list_takes_the_share_it_is_given() {
        let split = |percent| split_list_preview(0, 0, 100, 10, 80, 24, percent);

        assert_eq!(
            split(DEFAULT_LIST_PERCENT),
            (Rect::new(0, 0, 40, 10), Some(Rect::new(41, 0, 59, 10))),
            "the default share splits the body where it always has"
        );
        assert_eq!(
            split(60),
            (Rect::new(0, 0, 60, 10), Some(Rect::new(61, 0, 39, 10))),
            "a wider share moves the boundary and the preview takes the rest"
        );
        assert_eq!(
            split(5),
            (Rect::new(0, 0, 24, 10), Some(Rect::new(25, 0, 75, 10))),
            "a share under the minimum still leaves the list its floor"
        );
    }

    /// A body too narrow for two panes has no separator to drag, so the share is
    /// moot and the list keeps the full width whatever it says.
    #[test]
    fn a_narrow_body_ignores_the_share_and_stays_list_only() {
        assert_eq!(
            split_list_preview(0, 0, 79, 10, 80, 24, 60),
            (Rect::new(0, 0, 79, 10), None),
            "below the wide threshold the list takes everything"
        );
    }

    fn row_text(buf: &Buffer, row: u16, area: Rect) -> String {
        (area.x..area.x + area.width)
            .map(|c| buf[(c, row)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// A picklist over `base` with every row filtered in and no row selected, so
    /// the painter uses the plain row style.
    fn list_of(base: Vec<PathBuf>, match_indices: Vec<Vec<u32>>) -> PickList {
        PickList {
            filtered: (0..base.len()).collect(),
            base,
            match_indices,
            selected: usize::MAX,
            ..PickList::default()
        }
    }

    #[test]
    fn long_path_start_truncates_keeping_the_file_name() {
        let git_root = Path::new("/r");
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![]],
        );
        // Column width 20 leaves a 16-cell label area after the pad and the tag
        // slot.
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        assert_eq!(
            row_text(&buf, 0, area),
            " rs \u{2026}le/file_name.rs",
            "an ellipsis stands in for the dropped head, and the file name at the tail stays",
        );
    }

    /// A relative entry is a label rather than a path, such as a branch name of
    /// the palette's argument list, and its head names it. So a cut keeps the
    /// head, and a match in the cut tail paints nothing over the ellipsis.
    #[test]
    fn a_long_label_keeps_its_head() {
        let label = "feature/a-long-branch-name-for-the-cut";
        let list = list_of(vec![PathBuf::from(label)], vec![vec![0, 18]]);
        // Column width 20 leaves a 19-cell label area after the one-cell pad.
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            Path::new("/r"),
            None,
            "",
            &[],
            area,
            0,
            &match_theme(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            (row_text(&buf, 0, area), highlighted),
            (format!(" {}\u{2026}", &label[..18]), vec![1]),
        );
    }

    /// A row outside the repo abbreviates against the home the caller supplies,
    /// which is what keeps the row paint from resolving one itself.
    #[test]
    fn a_row_outside_the_repo_abbreviates_against_the_given_home() {
        let git_root = Path::new("/r");
        let list = list_of(vec![PathBuf::from("/fixture-home/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 24, 1);

        let paint = |home: Option<&Path>| {
            let mut buf = Buffer::empty(area);
            paint_path_rows(
                &list,
                git_root,
                home,
                "",
                &[],
                area,
                0,
                &Theme::empty(),
                Style::default(),
                &mut buf,
                &mut stoat_widgets::ApcScene::new(),
            );
            row_text(&buf, 0, area).trim().to_string()
        };

        assert_eq!(paint(Some(Path::new("/fixture-home"))), "rs ~/a.rs");
        assert_eq!(
            paint(None),
            "rs /fixture-home/a.rs",
            "and stays whole with no home to measure against"
        );
    }

    #[test]
    fn short_path_renders_in_full_without_an_ellipsis() {
        let git_root = Path::new("/r");
        let list = list_of(vec![PathBuf::from("/r/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let text = row_text(&buf, 0, area);
        assert!(
            !text.contains('\u{2026}'),
            "no ellipsis for a path that fits: {text:?}"
        );
        assert_eq!(text.trim(), "rs a.rs", "the whole path renders: {text:?}");
    }

    #[test]
    fn match_highlight_follows_the_truncated_tail() {
        let git_root = Path::new("/r");
        // The 42-char label loses its leading 27 chars in the 16-cell area past
        // the tag slot, so the tail starts at char 27. Match offset 0 lands in
        // the dropped head and offset 30 (the `f` of `file_name`) in the
        // visible tail.
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![0, 30]],
        );
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            0,
            &match_theme(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![8],
            "only the tail match highlights at its shifted column; the dropped-head match paints nothing"
        );
    }

    #[test]
    fn a_row_past_the_indexed_block_still_highlights() {
        let git_root = Path::new("/r");
        let base: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("/r/mod_{i:04}_alpha.rs")))
            .collect();

        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter("alpha", git_root);

        let deep = 550;
        assert!(
            deep >= list.indexed && deep < list.filtered.len(),
            "the painted row has to fall past the eagerly indexed block"
        );

        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            deep,
            &match_theme(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .count();
        assert_eq!(
            highlighted, 5,
            "the query's five characters highlight on a row whose offsets were derived at paint"
        );
    }

    #[test]
    fn an_anchored_listing_highlights_a_row_past_the_stored_block() {
        let git_root = Path::new("/r");
        let base: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("/r/docs/page_{i:04}.md")))
            .collect();

        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter("./docs", git_root);
        assert_eq!(
            list.match_indices.len(),
            INDEXED_ROWS,
            "the listing stores highlights for the leading block only"
        );

        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(
            &list,
            git_root,
            None,
            "",
            &[],
            area,
            550,
            &match_theme(),
            Style::default(),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![4, 5, 6, 7],
            "the anchor's four characters highlight on a row past the block"
        );
    }

    #[test]
    fn unselected_rows_take_the_caller_style() {
        let mut list = list_of(
            vec![PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs")],
            vec![vec![], vec![]],
        );
        list.selected = 1;
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        let muted = Color::Rgb(1, 2, 3);
        paint_path_rows(
            &list,
            Path::new("/r"),
            None,
            "",
            &[],
            area,
            0,
            &Theme::empty(),
            Style::default().fg(muted),
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let fg = |row: u16| -> Vec<Color> {
            (area.x..area.x + area.width)
                .map(|c| buf[(c, row)].fg)
                .collect()
        };
        assert_eq!(
            (fg(0), fg(1)),
            (vec![muted; 8], vec![Color::Reset; 8]),
            "the plain row paints in the caller's style and the selected row in the theme's",
        );
    }

    /// The box asks for a fixed size rather than growing with its candidates,
    /// so a terminal too small to host it gets no picker rather than a
    /// squeezed one.
    #[test]
    fn layout_is_none_when_the_terminal_cannot_host_the_box() {
        assert!(
            target_picker_layout(Rect::new(0, 0, 120, 40), 0, 40).is_some(),
            "a full-size terminal hosts it"
        );
        assert_eq!(
            (
                target_picker_layout(Rect::new(0, 0, 20, 40), 0, 40).is_none(),
                target_picker_layout(Rect::new(0, 0, 120, 4), 0, 40).is_none(),
            ),
            (true, true),
            "too narrow and too short both refuse rather than painting a stub"
        );
    }

    /// The width follows the window, so a 200-column area gets a box 160
    /// wide, 80 percent of the area.
    #[test]
    fn a_wide_window_widens_the_picker_box() {
        let layout = target_picker_layout(Rect::new(0, 0, 200, 60), 0, 40)
            .expect("the area hosts the picker");
        assert_eq!(layout.modal, Rect::new(20, 12, 160, 36));
    }
}
