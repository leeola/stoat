use super::TEXT_SCALE_POPUP;
use crate::toggle::{Toggle, ToggleStates};
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
/// The fewest columns of action text that a cut keeps.
///
/// A box too narrow to give every column this much draws nothing, because a key
/// beside a stub of its action tells the reader nothing.
const MIN_ACTION: usize = 8;

#[derive(Clone)]
pub(crate) struct HintsFooter {
    pub(crate) text: String,
    pub(crate) style: Style,
}

/// One row of the hints box, holding the keys that reach an action, the
/// action's label, and the toggle the action flips.
///
/// The row records which toggle it flips and never the state, so a flip
/// repaints the row with no rebuild.
pub(crate) struct HintRow {
    pub(crate) keys: String,
    pub(crate) action: String,
    pub(crate) toggle: Option<Toggle>,
}

/// A frame's grouped hint rows kept for reuse across frames.
///
/// `key` hashes the keymap-state inputs that decide which bindings are active,
/// so an unchanged key means the same rows and the keymap walk plus regrouping
/// can be skipped. A toggle's state is not part of the key, because each frame
/// reads it into the [`ToggleStates`] the paint takes.
pub(crate) struct HintsCache {
    pub(crate) key: u64,
    pub(crate) rows: Vec<HintRow>,
    /// The most recent layout of [`Self::rows`], or `None` before one is built.
    layout: Option<HintsLayout>,
}

impl HintsCache {
    pub(crate) fn new(key: u64, rows: Vec<HintRow>) -> Self {
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
/// box is always on over the conflict screen, so deriving that per frame would
/// rebuild a few hundred strings for a box that did not move.
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
/// because that is all the layout reads of them. A footer counting chunks
/// changes length as the reader advances, so its presence alone would not
/// catch a box that needs to be wider.
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
    /// Right-aligned key and indented action, ready to hand to the painter,
    /// with the toggle that marks the cell after the key.
    cells: Vec<LaidCell>,
    key_width: usize,
    action_width: usize,
}

/// One row of a [`LaidColumn`].
#[derive(Clone, Debug, PartialEq)]
struct LaidCell {
    key: String,
    action: String,
    toggle: Option<Toggle>,
}

/// Paint the hints box from pre-grouped [`HintRow`]s.
///
/// Takes rows rather than bindings because every caller holds a per-frame cache
/// keyed on the keymap state, and so paints an unchanged frame without
/// re-walking the keymap or regrouping. Run [`group_by_action`] first when
/// building rows fresh.
///
/// A row whose binding flips a toggle gets a mark directly right of its key,
/// on or off as `toggles` reads it. The mark changes no width and moves no
/// text in the box.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_hints_grouped(
    mode: &str,
    cache: &mut HintsCache,
    footer: Option<&HintsFooter>,
    toggles: ToggleStates,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
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
    let toggle_on = theme.get(crate::theme::scope::UI_TOGGLE_ACTIVE);
    let toggle_off = theme.get(crate::theme::scope::UI_TOGGLE_INACTIVE);
    let action_style = theme.get(crate::theme::scope::UI_TEXT);
    let end_x = inner.x + inner.width;
    let run_bg = crate::render::paint::style_rgb(
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    );

    let mut col_x = inner.x + 1;
    for column in &layout.columns {
        for (i, cell) in column.cells.iter().enumerate() {
            let row = inner.y + i as u16;
            if row >= inner.y + inner.height {
                break;
            }
            crate::render::chrome::text(
                buf,
                col_x,
                row,
                end_x,
                &cell.key,
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
                &cell.action,
                action_style,
                run_bg,
                TEXT_SCALE_POPUP,
                &mut *scene,
            );

            // The action text's fallback writes its leading spaces into the
            // mark's cell, so the mark goes last.
            if let Some(toggle) = cell.toggle {
                let on = toggles.is_on(toggle);
                crate::render::chrome::toggle_mark(
                    buf,
                    col_x + column.key_width as u16,
                    row,
                    on,
                    if on { toggle_on } else { toggle_off },
                    &mut *scene,
                );
            }
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
fn lay_out(rows: &[HintRow], key: LayoutKey) -> Option<HintsLayout> {
    let extra_rows = key.footer_len.map(|_| FOOTER_ROWS).unwrap_or(0);

    // Rows that fit vertically inside the box. The layout grows into extra
    // columns once the bindings would overflow this height.
    let available_rows = (key.area_height as usize).saturating_sub(BORDER_PAD + extra_rows);
    if available_rows == 0 {
        return None;
    }

    let col_count = rows.len().div_ceil(available_rows);
    let rows_per_col = rows.len().div_ceil(col_count);

    let chunks: Vec<&[HintRow]> = rows.chunks(rows_per_col).collect();
    let key_widths: Vec<usize> = chunks
        .iter()
        .map(|chunk| widest(chunk.iter().map(|row| &row.keys)))
        .collect();
    let action_widths: Vec<usize> = chunks
        .iter()
        .map(|chunk| widest(chunk.iter().map(|row| &row.action)))
        .collect();

    // A box wider than the area cuts its action text to fit, so every key stays
    // on screen. If the keys leave too little room for text, the box keeps its
    // text whole, lays out too wide, and draws nothing.
    let caps = action_caps(&key_widths, &action_widths, key.area_width as usize)
        .unwrap_or_else(|| action_widths.clone());

    let columns: Vec<LaidColumn> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let (key_width, cap) = (key_widths[i], caps[i]);
            let cells = chunk
                .iter()
                .map(|row| LaidCell {
                    key: format!("{:>key_width$}", row.keys),
                    action: format!("   {}", clip(&row.action, cap)),
                    toggle: row.toggle,
                })
                .collect();
            LaidColumn {
                cells,
                key_width,
                action_width: action_widths[i].min(cap),
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

/// The columns that the widest of `texts` spans, at one column per char as the
/// painter draws them.
fn widest<'a>(texts: impl Iterator<Item = &'a String>) -> usize {
    texts.map(|text| text.chars().count()).max().unwrap_or(0)
}

/// The widest action text each column keeps, so a box of these columns fits
/// `area_width`.
///
/// Columns whose actions are short keep them whole and leave the rest of their
/// share to the columns with longer ones, so a cut lands only where the text is
/// long. `None` when some column keeps neither its whole action text nor
/// [`MIN_ACTION`] columns of it.
fn action_caps(
    key_widths: &[usize],
    action_widths: &[usize],
    area_width: usize,
) -> Option<Vec<usize>> {
    let fixed = BORDER_PAD
        + CONTENT_PAD
        + INTER_COL_GAP * key_widths.len().saturating_sub(1)
        + key_widths.iter().map(|width| width + GAP).sum::<usize>();
    let mut budget = area_width.checked_sub(fixed)?;

    let mut by_width: Vec<usize> = (0..action_widths.len()).collect();
    by_width.sort_by_key(|&column| action_widths[column]);
    let mut caps = vec![0; action_widths.len()];
    for (placed, &column) in by_width.iter().enumerate() {
        let share = budget / (by_width.len() - placed);
        caps[column] = action_widths[column].min(share);
        budget -= caps[column];
    }

    caps.iter()
        .zip(action_widths)
        .all(|(&cap, &width)| cap >= width.min(MIN_ACTION))
        .then_some(caps)
}

/// `text` cut to `cap` columns, ending in an ellipsis when it was longer.
fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(cap.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// Collapses entries that share an action description, joining their keys with
/// `", "` in first-seen order. Ensures each action appears on exactly one row.
///
/// Each entry is a key, its action's label, and the toggle the binding flips.
/// A merged row keeps the toggle of its first binding.
pub(crate) fn group_by_action(bindings: &[(&str, String, Option<Toggle>)]) -> Vec<HintRow> {
    let mut rows: Vec<HintRow> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (key, action, toggle) in bindings {
        let action = action.as_str();
        if let Some(&i) = index.get(action) {
            let row = &mut rows[i];
            row.keys.push_str(", ");
            row.keys.push_str(key);
        } else {
            index.insert(action, rows.len());
            rows.push(HintRow {
                keys: key.to_string(),
                action: action.to_string(),
                toggle: *toggle,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{
        action_caps, group_by_action, render_hints_grouped, HintsCache, HintsFooter, LaidCell,
    };
    use crate::{
        action_handlers,
        keymap::CompiledKey,
        test_harness::TestHarness,
        theme::Theme,
        toggle::{Toggle, ToggleStates},
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::{buffer::Buffer, layout::Rect};

    fn row_text(buf: &Buffer, y: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn render(bindings: &[(&str, String)], width: u16, height: u16) -> Buffer {
        let mut cache = HintsCache::new(0, group_by_action(&plain(bindings)));
        render_into(&mut cache, None, ToggleStates::default(), width, height)
    }

    /// `bindings` as entries whose binding flips no toggle.
    fn plain<'a>(bindings: &[(&'a str, String)]) -> Vec<(&'a str, String, Option<Toggle>)> {
        bindings
            .iter()
            .map(|(key, action)| (*key, action.clone(), None))
            .collect()
    }

    /// Paint `cache` at the given size, laying it out first if it needs it.
    fn render_into(
        cache: &mut HintsCache,
        footer: Option<&HintsFooter>,
        toggles: ToggleStates,
        width: u16,
        height: u16,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        // An empty theme resolves no RGB colours, so the helpers still take their
        // cell fallback and the assertions below read real glyphs.
        let mut scene = stoat_widgets::ApcScene::new();
        render_hints_grouped(
            "normal",
            cache,
            footer,
            toggles,
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
        let mut cache = HintsCache::new(7, group_by_action(&plain(&bindings)));

        let first = render_into(&mut cache, None, ToggleStates::default(), 40, 20);
        let laid_out: Vec<Vec<LaidCell>> = cache
            .layout
            .as_ref()
            .expect("the first paint lays out")
            .columns
            .iter()
            .map(|column| column.cells.clone())
            .collect();

        let second = render_into(&mut cache, None, ToggleStates::default(), 40, 20);
        assert_eq!(
            second.content, first.content,
            "a repaint with nothing changed paints the same cells",
        );

        let after: Vec<Vec<LaidCell>> = cache
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
        let mut cache = HintsCache::new(7, group_by_action(&plain(&bindings)));

        let footer = |text: &str| HintsFooter {
            text: text.to_string(),
            style: Default::default(),
        };

        render_into(
            &mut cache,
            Some(&footer("1/9")),
            ToggleStates::default(),
            60,
            20,
        );
        let narrow = cache.layout.as_ref().expect("laid out").box_width;

        render_into(
            &mut cache,
            Some(&footer(
                "a footer far longer than the single binding above it",
            )),
            ToggleStates::default(),
            60,
            20,
        );
        let wide = cache.layout.as_ref().expect("laid out again").box_width;

        assert!(
            wide > narrow,
            "the longer footer must widen the box, got {narrow} then {wide}",
        );
    }

    /// A long action text is cut to fit the area, and its key stays on screen.
    /// A short action text in the same column stays whole.
    #[test]
    fn a_box_too_wide_cuts_its_action_text_to_fit() {
        let long = "x".repeat(60);
        let buf = render(&[("a", long), ("b", "short".to_string())], 40, 10);

        let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        let cut = format!("a   {}\u{2026}", "x".repeat(31));
        assert_eq!(
            (
                rows.iter().any(|row| row.contains(&cut)),
                rows.iter().any(|row| row.contains("b   short")),
            ),
            (true, true),
            "the long text ends in an ellipsis at the box edge: {rows:#?}",
        );
    }

    #[test]
    fn short_actions_keep_their_width_and_leave_the_rest_to_long_ones() {
        assert_eq!(action_caps(&[1, 1], &[60, 5], 40), Some(vec![20, 5]));
    }

    /// One column with a one-column key spends 8 columns on its frame, padding,
    /// key, and gap, so an area of 16 leaves exactly 8 for its action text.
    #[test]
    fn a_cut_keeps_at_least_eight_columns_of_action_text() {
        assert_eq!(
            (action_caps(&[1], &[60], 16), action_caps(&[1], &[60], 15)),
            (Some(vec![8]), None),
        );
    }

    #[test]
    fn a_toggle_row_marks_the_cell_after_its_key_and_moves_no_text() {
        let paint = |toggle: Option<Toggle>, toggles: ToggleStates| {
            let bindings = [
                ("f", "follow".to_string(), toggle),
                ("n", "next".to_string(), None),
            ];
            let mut cache = HintsCache::new(0, group_by_action(&bindings));
            let buf = render_into(&mut cache, None, toggles, 40, 20);
            (0..buf.area.height)
                .map(|y| row_text(&buf, y))
                .collect::<Vec<String>>()
                .join("\n")
        };
        let on = paint(
            Some(Toggle::FollowChanges),
            [Toggle::FollowChanges].into_iter().collect(),
        );
        let off = paint(Some(Toggle::FollowChanges), ToggleStates::default());
        let unmarked = paint(None, ToggleStates::default());

        assert_eq!(
            (
                on.contains("f\u{25aa}  follow"),
                off.contains("f\u{25ab}  follow"),
                unmarked.contains("f   follow"),
            ),
            (true, true, true),
            "the mark sits in the cell after the key:\n{on}\n{off}\n{unmarked}",
        );
        assert_eq!(
            (on.replace('\u{25aa}', " "), off.replace('\u{25ab}', " ")),
            (unmarked.clone(), unmarked),
            "the mark moves no text in the box",
        );
    }

    #[test]
    fn box_too_wide_for_the_area_renders_nothing() {
        let keys: Vec<String> = (0..40).map(|i| format!("k{i:02}")).collect();
        let buf = render(&numbered_bindings(&keys), 30, 5);

        let painted = (0..buf.area.height).any(|y| !row_text(&buf, y).trim().is_empty());
        assert!(!painted, "a box too wide for the area paints nothing");
    }

    /// A dial binding names a view, so the box lists the five chords beside
    /// the other context keys of a diff surface, and none of them on a plain
    /// pane, where no dial answers.
    #[test]
    fn the_hints_box_lists_the_diff_chords_on_a_diff_surface() {
        const DIALS: [(char, &str); 5] = [
            ('6', "toggle underline on diff change spans"),
            ('7', "toggle bold on diff change spans"),
            ('8', "toggle syntax colors in the diff view"),
            ('9', "lower the diff tint"),
            ('0', "raise the diff tint"),
        ];
        let mut h = TestHarness::default();
        h.stoat.key_hints_visible = true;
        let mut dial_rows = |diff_view: bool| {
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("editor")
                .set_diff_view(diff_view);
            h.snapshot();
            h.stoat
                .hints_cache
                .as_ref()
                .expect("the hints box laid out its rows")
                .rows
                .iter()
                .filter(|row| DIALS.iter().any(|(_, action)| row.action == *action))
                .map(|row| (row.keys.clone(), row.action.clone()))
                .collect::<Vec<_>>()
        };
        let chord_label = |digit| {
            CompiledKey {
                code: KeyCode::Char(digit),
                modifiers: KeyModifiers::SUPER,
                any_digit: false,
                wheel: None,
                side_button: None,
            }
            .display_label()
        };

        assert_eq!(
            (dial_rows(true), dial_rows(false)),
            (
                DIALS
                    .iter()
                    .map(|&(digit, action)| (chord_label(digit), action.to_string()))
                    .collect::<Vec<_>>(),
                Vec::new(),
            ),
            "the diff view lists each dial under its chord, and a plain pane lists none"
        );
    }
}
