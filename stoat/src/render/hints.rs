use super::TEXT_SCALE_POPUP;
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::collections::HashMap;

/// Columns between a row's key and its action.
const GAP: usize = 3;
/// Columns between one column's action and the next column's key.
const INTER_COL_GAP: usize = 3;
/// Rows and columns the frame itself occupies.
const BORDER_PAD: usize = 2;
/// Columns of breathing room inside the frame.
const CONTENT_PAD: usize = 2;
/// Rows a footer adds, being a separator and the text under it.
const FOOTER_ROWS: usize = 2;

#[derive(Clone)]
pub(crate) struct HintsFooter {
    pub(crate) text: String,
    pub(crate) style: Style,
}

/// A frame's grouped hint rows kept for reuse across frames.
///
/// `key` hashes the keymap-state inputs that decide which bindings are active,
/// so an unchanged key means the same rows and the keymap walk plus regrouping
/// can be skipped.
pub(crate) struct HintsCache {
    pub(crate) key: u64,
    pub(crate) rows: Vec<(String, String)>,
    /// The most recent layout of [`Self::rows`], or `None` before one is built.
    layout: Option<HintsLayout>,
}

impl HintsCache {
    pub(crate) fn new(key: u64, rows: Vec<(String, String)>) -> Self {
        Self {
            key,
            rows,
            layout: None,
        }
    }
}

/// The hints box arranged into columns, with every cell's text already padded.
///
/// Laying out rescans each column's widths and formats two strings per row. The
/// box is always on over the review and conflict screens, so deriving that per
/// frame would rebuild a few hundred strings for a box that did not move.
struct HintsLayout {
    /// Everything the layout was derived from, so a frame that changed none of
    /// it paints from what is here.
    key: LayoutKey,
    columns: Vec<LaidColumn>,
    /// Rows in the tallest column, which the footer separator sits below.
    max_col_rows: usize,
    box_width: u16,
    box_height: u16,
}

/// The inputs [`HintsLayout`] is derived from.
///
/// The footer and title contribute their lengths rather than their text,
/// because that is all the layout reads of them. A review footer counting
/// chunks changes length as the review advances, so its presence alone would
/// not catch a box that needs to be wider.
#[derive(PartialEq, Eq)]
struct LayoutKey {
    rows: u64,
    area_width: u16,
    area_height: u16,
    title_len: usize,
    footer_len: Option<usize>,
}

/// One column of the box, holding each row's text as it will be painted.
struct LaidColumn {
    /// Right-aligned key and indented action, ready to hand to the painter.
    cells: Vec<(String, String)>,
    key_width: usize,
    action_width: usize,
}

/// Paint the hints box from pre-grouped `(keys, action)` rows.
///
/// Takes rows rather than bindings because every caller holds a per-frame cache
/// keyed on the keymap state, and so paints an unchanged frame without
/// re-walking the keymap or regrouping. Run [`group_by_action`] first when
/// building rows fresh.
pub(crate) fn render_hints_grouped(
    mode: &str,
    cache: &mut HintsCache,
    footer: Option<&HintsFooter>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    if cache.rows.is_empty() || area.width < 10 || area.height < 4 {
        return;
    }

    // Reserve the bottom row for the pane status bar. Every caller passes the
    // full window, so the box lays out flush to the right edge above the bar.
    let area = super::hints_overlay_area(area);

    let key = LayoutKey {
        rows: cache.key,
        area_width: area.width,
        area_height: area.height,
        title_len: mode.len(),
        footer_len: footer.map(|f| f.text.len()),
    };
    if cache.layout.as_ref().map(|layout| &layout.key) != Some(&key) {
        cache.layout = lay_out(&cache.rows, key);
    }

    // A box the area cannot hold is cached as readily as one it can, so a
    // window too small stops re-laying out every frame.
    let Some(layout) = cache.layout.as_ref() else {
        return;
    };
    let (box_width, box_height) = (layout.box_width, layout.box_height);
    let max_col_rows = layout.max_col_rows;
    if box_width > area.width || box_height > area.height {
        return;
    }

    let x = area.x + area.width.saturating_sub(box_width);
    let y = area.y + area.height.saturating_sub(box_height);
    let help_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let title = format!(" {mode} ");
    crate::render::clear_themed(help_area, buf, theme);
    // Above the pools, because this box is declared after every modal and so sits
    // over the commit picker's list and preview surfaces. Layered with the grid it
    // would vanish under their composites for the length of every glide.
    let inner = crate::render::chrome::modal_frame_above_pools(
        buf,
        help_area,
        Some(title.as_str()),
        modal_style,
        theme,
        &mut *scene,
    );

    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let action_style = theme.get(crate::theme::scope::UI_TEXT);
    let end_x = inner.x + inner.width;
    let run_bg = crate::render::review::style_rgb(
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    );

    let mut col_x = inner.x + 1;
    for column in &layout.columns {
        for (i, (padded_key, action_text)) in column.cells.iter().enumerate() {
            let row = inner.y + i as u16;
            if row >= inner.y + inner.height {
                break;
            }
            crate::render::chrome::text(
                buf,
                col_x,
                row,
                end_x,
                padded_key,
                key_style,
                run_bg,
                TEXT_SCALE_POPUP,
                &mut *scene,
            );

            crate::render::chrome::text(
                buf,
                col_x + column.key_width as u16,
                row,
                end_x,
                action_text,
                action_style,
                run_bg,
                TEXT_SCALE_POPUP,
                &mut *scene,
            );
        }
        col_x += (column.key_width + GAP + column.action_width + INTER_COL_GAP) as u16;
    }

    if let Some(footer) = footer {
        let sep_row = inner.y + max_col_rows as u16;
        let text_row = sep_row + 1;
        if sep_row < inner.y + inner.height {
            let sep_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
            crate::render::chrome::hline(
                buf,
                inner.x,
                sep_row,
                inner.width,
                sep_style,
                &mut *scene,
            );
        }
        if text_row < inner.y + inner.height {
            crate::render::chrome::text(
                buf,
                inner.x + 1,
                text_row,
                end_x,
                &footer.text,
                footer.style,
                run_bg,
                TEXT_SCALE_POPUP,
                scene,
            );
        }
    }
}

/// Arrange `rows` into columns that fit `key`'s area, padding every cell.
///
/// `None` when the area leaves no room for a single row, which is the one case
/// that has nothing to lay out rather than something too large to show. A box
/// wider or taller than the area still lays out, so the caller can cache that it
/// does not fit.
fn lay_out(rows: &[(String, String)], key: LayoutKey) -> Option<HintsLayout> {
    let extra_rows = key.footer_len.map(|_| FOOTER_ROWS).unwrap_or(0);

    // Rows that fit vertically inside the box. The layout grows into extra
    // columns once the bindings would overflow this height.
    let available_rows = (key.area_height as usize).saturating_sub(BORDER_PAD + extra_rows);
    if available_rows == 0 {
        return None;
    }

    let col_count = rows.len().div_ceil(available_rows);
    let rows_per_col = rows.len().div_ceil(col_count);

    let columns: Vec<LaidColumn> = rows
        .chunks(rows_per_col)
        .map(|chunk| {
            let key_width = chunk.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            let action_width = chunk.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
            let cells = chunk
                .iter()
                .map(|(k, a)| (format!("{k:>key_width$}"), format!("   {a}")))
                .collect();
            LaidColumn {
                cells,
                key_width,
                action_width,
            }
        })
        .collect();

    let max_col_rows = columns.iter().map(|c| c.cells.len()).max().unwrap_or(0);
    let columns_width: usize = columns
        .iter()
        .map(|c| c.key_width + GAP + c.action_width)
        .sum::<usize>()
        + INTER_COL_GAP * columns.len().saturating_sub(1);

    let title_width = key.title_len + 4;
    let footer_width = key.footer_len.unwrap_or(0);
    let content_width = columns_width.max(title_width).max(footer_width);

    Some(HintsLayout {
        box_width: (content_width + BORDER_PAD + CONTENT_PAD) as u16,
        box_height: (max_col_rows + BORDER_PAD + extra_rows) as u16,
        key,
        columns,
        max_col_rows,
    })
}

/// Collapses entries that share an action description, joining their keys with
/// `", "` in first-seen order. Ensures each action appears on exactly one row.
pub(crate) fn group_by_action(bindings: &[(&str, String)]) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (key, action) in bindings {
        let action = action.as_str();
        if let Some(&i) = index.get(action) {
            let row = &mut rows[i];
            row.0.push_str(", ");
            row.0.push_str(key);
        } else {
            index.insert(action, rows.len());
            rows.push((key.to_string(), action.to_string()));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{group_by_action, render_hints_grouped, HintsCache, HintsFooter};
    use crate::theme::Theme;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn row_text(buf: &Buffer, y: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn render(bindings: &[(&str, String)], width: u16, height: u16) -> Buffer {
        let mut cache = HintsCache::new(0, group_by_action(bindings));
        render_into(&mut cache, None, width, height)
    }

    /// Paint `cache` at the given size, laying it out first if it needs it.
    fn render_into(
        cache: &mut HintsCache,
        footer: Option<&HintsFooter>,
        width: u16,
        height: u16,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        // An empty theme resolves no RGB colours, so the helpers still take their
        // cell fallback and the assertions below read real glyphs.
        let mut scene = stoatty_widgets::ApcScene::new();
        render_hints_grouped(
            "normal",
            cache,
            footer,
            &Theme::empty(),
            area,
            &mut buf,
            &mut scene,
        );
        buf
    }

    fn numbered_bindings(keys: &[String]) -> Vec<(&str, String)> {
        keys.iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), format!("act{i:02}")))
            .collect()
    }

    #[test]
    fn few_bindings_stay_single_column() {
        let bindings = vec![
            ("k0", "act0".to_string()),
            ("k1", "act1".to_string()),
            ("k2", "act2".to_string()),
        ];
        let buf = render(&bindings, 40, 20);

        let row_of =
            |needle: &str| (0..buf.area.height).find(|&y| row_text(&buf, y).contains(needle));
        let (r0, r1, r2) = (row_of("act0"), row_of("act1"), row_of("act2"));
        assert!(
            r0.is_some() && r1.is_some() && r2.is_some(),
            "every binding renders",
        );
        assert!(
            r0 != r1 && r1 != r2 && r0 != r2,
            "a single column stacks each binding on its own row",
        );
    }

    #[test]
    fn overflowing_rows_wrap_into_columns() {
        let keys: Vec<String> = (0..40).map(|i| format!("k{i:02}")).collect();
        let buf = render(&numbered_bindings(&keys), 80, 15);

        let side_by_side = (0..buf.area.height).any(|y| {
            let text = row_text(&buf, y);
            text.contains("k00") && text.contains("k10")
        });
        assert!(
            side_by_side,
            "the first rows of two columns share a buffer row",
        );
    }

    /// The padded strings are what a repaint is meant to stop rebuilding, so an
    /// unchanged frame has to reach the paint without touching them.
    #[test]
    fn an_unchanged_frame_paints_from_the_laid_out_strings() {
        let bindings = vec![("k", "act".to_string()), ("kk", "other".to_string())];
        let mut cache = HintsCache::new(7, group_by_action(&bindings));

        let first = render_into(&mut cache, None, 40, 20);
        let laid_out: Vec<Vec<(String, String)>> = cache
            .layout
            .as_ref()
            .expect("the first paint lays out")
            .columns
            .iter()
            .map(|column| column.cells.clone())
            .collect();

        let second = render_into(&mut cache, None, 40, 20);
        assert_eq!(
            second.content, first.content,
            "a repaint with nothing changed paints the same cells",
        );

        let after: Vec<Vec<(String, String)>> = cache
            .layout
            .as_ref()
            .expect("the layout survives the repaint")
            .columns
            .iter()
            .map(|column| column.cells.clone())
            .collect();
        assert_eq!(after, laid_out, "and reuses the strings it laid out before");
    }

    /// The footer's length sets the box width, so a footer that outgrows the
    /// columns has to widen the box rather than be clipped by a stale layout.
    #[test]
    fn a_longer_footer_widens_the_box() {
        let bindings = vec![("k", "act".to_string())];
        let mut cache = HintsCache::new(7, group_by_action(&bindings));

        let footer = |text: &str| HintsFooter {
            text: text.to_string(),
            style: Default::default(),
        };

        render_into(&mut cache, Some(&footer("1/9")), 60, 20);
        let narrow = cache.layout.as_ref().expect("laid out").box_width;

        render_into(
            &mut cache,
            Some(&footer(
                "a footer far longer than the single binding above it",
            )),
            60,
            20,
        );
        let wide = cache.layout.as_ref().expect("laid out again").box_width;

        assert!(
            wide > narrow,
            "the longer footer must widen the box, got {narrow} then {wide}",
        );
    }

    #[test]
    fn box_too_wide_for_the_area_renders_nothing() {
        let keys: Vec<String> = (0..40).map(|i| format!("k{i:02}")).collect();
        let buf = render(&numbered_bindings(&keys), 30, 5);

        let painted = (0..buf.area.height).any(|y| !row_text(&buf, y).trim().is_empty());
        assert!(!painted, "a box too wide for the area paints nothing");
    }
}
