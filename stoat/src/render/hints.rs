use crate::{
    theme::TypeRole,
    toggle::{Toggle, ToggleStates},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders},
};
use std::collections::HashMap;
use stoat_widgets::panel::Panel;
use stoatty_protocol::command::{
    panel::{CORNER_TOP_LEFT, FILLET_BOTTOM_LEFT},
    BorderStyle, Edge, PanelShadow,
};

/// Columns between a row's key and its action.
const GAP: usize = 3;
/// Columns between one column's action and the next column's key.
const INTER_COL_GAP: usize = 3;
/// Rows and columns the frame itself occupies.
const BORDER_PAD: usize = 2;
/// Columns of breathing room inside the frame.
const CONTENT_PAD: usize = 2;
/// Rows the footer adds, being a separator and the text under it.
const FOOTER_ROWS: usize = 2;
/// The most rows a column holds while the columns fit the area's width, so a
/// long list spreads into columns rather than into one column the window's
/// height.
const MAX_COLUMN_ROWS: usize = 8;
/// The fewest columns of action text that a cut keeps.
///
/// A box too narrow to give every column this much draws nothing, because a key
/// beside a stub of its action tells the reader nothing.
const MIN_ACTION: usize = 8;
/// The row every one-char chord box ends in. Each chord drops on a key it does
/// not take, so Esc cancels any of them.
const CANCEL_ROW: (&str, &str) = ("Esc", "cancel");

#[derive(Clone)]
pub(crate) struct HintsFooter {
    pub(crate) text: String,
    pub(crate) style: Style,
}

/// The keys the footer names at its right, the one that hides the box and the
/// one that opens the full help.
///
/// Either is `None` when no key reaches its action, and the footer then
/// leaves that part out.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct FooterKeys {
    pub(crate) hide: Option<String>,
    pub(crate) help: Option<String>,
}

/// The box an armed one-char chord shows while it waits on its char.
///
/// `rows` pairs each char the chord takes with the label its row reads, and
/// chars that share a label fold into one row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChordHints {
    pub(crate) title: &'static str,
    pub(crate) rows: &'static [(&'static str, &'static str)],
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
    /// Whether the row's keys open a submenu, which its label names as
    /// `+name` and the box paints in `ui.hints.submenu`.
    pub(crate) submenu: bool,
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
    /// The keys the footer names, which the same keymap state decides as
    /// [`Self::rows`].
    pub(crate) footer_keys: FooterKeys,
    /// The most recent layout of [`Self::rows`], or `None` before one is built.
    layout: Option<HintsLayout>,
}

impl HintsCache {
    pub(crate) fn new(key: u64, rows: Vec<HintRow>, footer_keys: FooterKeys) -> Self {
        Self {
            key,
            rows,
            footer_keys,
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
/// The title, the subtitle, and the footer's two texts contribute their
/// lengths rather than their text, because that is all the layout reads of
/// them. A footer counting chunks changes length as the reader advances, so
/// its presence alone does not catch a box that needs to be wider.
#[derive(PartialEq, Eq)]
struct LayoutKey {
    rows: u64,
    area_width: u16,
    area_height: u16,
    title_len: usize,
    subtitle_len: usize,
    /// The footer's own text, 0 when the caller passes none.
    footer_len: usize,
    /// The keys and words at the footer's right.
    right_len: usize,
}

impl LayoutKey {
    /// The rows the footer takes, none for a footer with no text and no keys,
    /// so that box ends at its last row rather than in a blank one.
    fn footer_rows(&self) -> usize {
        match (self.footer_len, self.right_len) {
            (0, 0) => 0,
            _ => FOOTER_ROWS,
        }
    }
}

/// One column of the box, holding each row's text as it will be painted.
struct LaidColumn {
    /// Right-aligned key and indented action, ready to hand to the painter,
    /// with the toggle that marks the cell after the key.
    cells: Vec<LaidCell>,
    key_width: usize,
    action_width: usize,
}

/// How a set of [`LaidColumn`]s fits the area's width, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    /// Every action text shows whole.
    Whole,
    /// Some action text is cut so the columns fit.
    Cut,
    /// Even cut text leaves the columns wider than the area.
    TooWide,
}

/// One row of a [`LaidColumn`].
#[derive(Clone, Debug, PartialEq)]
struct LaidCell {
    key: String,
    action: String,
    toggle: Option<Toggle>,
    submenu: bool,
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
///
/// The box opens on a title row, `title` at its left and `subtitle` in muted
/// text at its right. A footer always closes it, with `footer`'s text at its
/// left and the cache's [`FooterKeys`] at its right. Under a live scene the box
/// is the docked card, a borderless panel in `ui.modal.hints.card`'s fill.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_hints_grouped(
    title: &str,
    subtitle: &str,
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

    // Reserve the bottom row for the window's status bar. Every caller passes the
    // full window, so the box lays out flush to the right edge above the bar.
    let area = super::hints_overlay_area(area);

    let right = footer_runs(&cache.footer_keys);
    let right_width = runs_width(&right);
    let key = LayoutKey {
        rows: cache.key,
        area_width: area.width,
        area_height: area.height,
        title_len: title.chars().count(),
        subtitle_len: subtitle.chars().count(),
        footer_len: footer.map_or(0, |f| f.text.chars().count()),
        right_len: right_width,
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

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS_CARD);
    let run_bg = crate::render::modal_fill(theme, modal_style);
    let docked = run_bg.filter(|_| scene.live());
    let inner = docked_frame(buf, help_area, modal_style, docked, theme, &mut *scene);

    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    // A row's keys are what the box is for, so they draw bold, and the
    // footer's keys keep the text's weight.
    let row_key_style = key_style.add_modifier(Modifier::BOLD);
    let toggle_on = theme.get(crate::theme::scope::UI_TOGGLE_ACTIVE);
    let toggle_off = theme.get(crate::theme::scope::UI_TOGGLE_INACTIVE);
    let action_style = theme.get(crate::theme::scope::UI_TEXT);
    // A theme with no submenu style still sets the submenus apart, in the keys'
    // color.
    let submenu_style = theme
        .try_get(crate::theme::scope::UI_HINTS_SUBMENU)
        .unwrap_or(key_style);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let rule_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
    let end_x = inner.x + inner.width;
    let row_scale = theme.type_scale(TypeRole::Ui);
    let mark_scale = theme.type_scale(TypeRole::Small);

    // The title row is the frame's top row. A cell frame's border rules that
    // row off by itself, so only the card draws the hairline under it.
    if docked.is_some() {
        crate::render::chrome::hline_edge(
            buf,
            inner.x,
            help_area.y,
            inner.width,
            rule_style,
            &mut *scene,
        );
    }
    // The texts take a space each side, so a cell frame's border reads them
    // apart from its own line.
    crate::render::chrome::text(
        buf,
        inner.x,
        help_area.y,
        end_x,
        &format!(" {title} "),
        action_style,
        run_bg,
        row_scale,
        &mut *scene,
    );
    if !subtitle.is_empty() {
        let subtitle = format!(" {subtitle} ");
        crate::render::chrome::text(
            buf,
            end_x.saturating_sub(subtitle.chars().count() as u16),
            help_area.y,
            end_x,
            &subtitle,
            muted_style,
            run_bg,
            row_scale,
            &mut *scene,
        );
    }

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
                row_key_style,
                run_bg,
                row_scale,
                &mut *scene,
            );

            crate::render::chrome::text(
                buf,
                col_x + column.key_width as u16,
                row,
                end_x,
                &cell.action,
                match cell.submenu {
                    true => submenu_style,
                    false => action_style,
                },
                run_bg,
                row_scale,
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
                    mark_scale,
                    &mut *scene,
                );
            }
        }
        col_x += (column.key_width + GAP + column.action_width + INTER_COL_GAP) as u16;
    }

    // The footer rows follow the tallest column. The separator rules off the
    // text row's top edge.
    if layout.key.footer_rows() == 0 {
        return;
    }
    let sep_row = inner.y + max_col_rows as u16;
    let text_row = sep_row + 1;
    crate::render::chrome::hline_edge(buf, inner.x, sep_row, inner.width, rule_style, &mut *scene);
    if let Some(footer) = footer {
        // The rich text run ignores `end_x`, so the text itself is cut to the
        // box, short of the keys at the row's right.
        let content_width = (box_width as usize).saturating_sub(BORDER_PAD + CONTENT_PAD);
        let room = match right_width {
            0 => content_width,
            right => content_width.saturating_sub(right + GAP),
        };
        crate::render::chrome::text(
            buf,
            inner.x + 1,
            text_row,
            end_x,
            &clip(&footer.text, room),
            footer.style,
            run_bg,
            row_scale,
            &mut *scene,
        );
    }
    let mut x = end_x.saturating_sub(1 + right_width as u16);
    for (text, is_key) in right {
        let style = match is_key {
            true => key_style,
            false => muted_style,
        };
        crate::render::chrome::text(
            buf,
            x,
            text_row,
            end_x,
            text,
            style,
            run_bg,
            row_scale,
            &mut *scene,
        );
        x += text.chars().count() as u16;
    }
}

/// Draw the hints box's frame over `area` and return the rect one cell inside
/// it.
///
/// `docked` is the card's fill, `Some` only under a live scene with an RGB
/// fill. The box is then the docked card, a borderless panel in that fill. It
/// rounds its top-left corner alone, a fillet joins its bottom-left corner to
/// the status bar it docks on, and its shadow falls above and left of it alone.
/// Otherwise it is the rounded modal frame in `style`. The inside rect is the
/// same in both, so the layout's [`BORDER_PAD`] holds for either.
///
/// Both clear the cells under the box first, because a panel's fill covers the
/// cells' backgrounds and not their glyphs.
fn docked_frame(
    buf: &mut Buffer,
    area: Rect,
    style: Style,
    docked: Option<[u8; 3]>,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) -> Rect {
    crate::render::clear_modal(area, buf, theme, style);
    // Above the pools, because this box is declared after every modal and so sits
    // over the commit picker's list and preview surfaces. Layered with the grid,
    // it vanishes under their composites for the length of every glide.
    let Some(fill) = docked else {
        return crate::render::chrome::modal_frame_above_pools(
            buf, area, None, style, theme, scene,
        );
    };
    Panel {
        style: BorderStyle::None_,
        border: fill,
        corner_radius: 2,
        fill: Some(fill),
        shadow: PanelShadow::Halo,
        edges: Edge::symmetric(0, 0),
        corners: CORNER_TOP_LEFT,
        fillets: FILLET_BOTTOM_LEFT,
        fillet_radius: 3,
        hatch: None,
        above_pools: true,
        backdrop: false,
        anchor: None,
    }
    .draw_components(area, scene);
    Block::default().borders(Borders::ALL).inner(area)
}

/// The footer's right text as runs, each a key or the plain words after one.
///
/// The text names the key that hides the box and the key that opens the full
/// help, each followed by its word. A key the keymap does not reach leaves its
/// part out, and the dot between the two parts goes with either. A left-out
/// part is an empty run.
fn footer_runs(keys: &FooterKeys) -> [(&str, bool); 5] {
    let (hide, help) = (keys.hide.as_deref(), keys.help.as_deref());
    let both = hide.is_some() && help.is_some();
    [
        (hide.unwrap_or(""), true),
        (if hide.is_some() { " hide" } else { "" }, false),
        (if both { " \u{b7} " } else { "" }, false),
        (help.unwrap_or(""), true),
        (if help.is_some() { " help" } else { "" }, false),
    ]
}

/// The columns `runs` span, at one column per char as the painter draws them.
fn runs_width(runs: &[(&str, bool)]) -> usize {
    runs.iter().map(|(text, _)| text.chars().count()).sum()
}

/// Arrange `rows` into columns that fit `key`'s area, padding every cell.
///
/// `None` when the area leaves no room for a single row, which is the one case
/// that has nothing to lay out rather than something too large to show. A box
/// wider or taller than the area still lays out, so the caller can cache that it
/// does not fit.
fn lay_out(rows: &[HintRow], key: LayoutKey) -> Option<HintsLayout> {
    // Rows that fit vertically inside the box. The layout grows into extra
    // columns once the bindings overflow this height.
    let footer_rows = key.footer_rows();
    let available_rows = (key.area_height as usize).saturating_sub(BORDER_PAD + footer_rows);
    if available_rows == 0 {
        return None;
    }

    // Columns of a capped height spread wider than a tall column does. Where
    // they cut their text to fit the area, or overflow it, taller columns that
    // fit better take their place, the shortest such, so a narrow window keeps
    // whole labels and a box that shows.
    let capped = available_rows.min(MAX_COLUMN_ROWS);
    let mut best = lay_out_columns(rows, capped, key.area_width);
    for rows_cap in capped + 1..=available_rows {
        if best.1 == Fit::Whole {
            break;
        }
        let taller = lay_out_columns(rows, rows_cap, key.area_width);
        if taller.1 < best.1 {
            best = taller;
        }
    }
    let columns = best.0;

    let max_col_rows = columns.iter().map(|c| c.cells.len()).max().unwrap_or(0);
    let columns_width: usize = columns
        .iter()
        .map(|c| c.key_width + GAP + c.action_width)
        .sum::<usize>()
        + INTER_COL_GAP * columns.len().saturating_sub(1);

    let title_width = match key.subtitle_len {
        0 => key.title_len,
        subtitle => key.title_len + GAP + subtitle,
    };
    // The footer never widens the box past the area. The paint cuts the text
    // that does not fit, so a long footer never hides the keys above it.
    let footer_width = match (key.footer_len, key.right_len) {
        (left, 0) => left,
        (0, right) => right,
        (left, right) => left + GAP + right,
    }
    .min((key.area_width as usize).saturating_sub(BORDER_PAD + CONTENT_PAD));
    let content_width = columns_width.max(title_width).max(footer_width);

    Some(HintsLayout {
        box_width: (content_width + BORDER_PAD + CONTENT_PAD) as u16,
        box_height: (max_col_rows + BORDER_PAD + footer_rows) as u16,
        key,
        columns,
        max_col_rows,
    })
}

/// Arrange `rows` into columns of at most `rows_cap` rows, padding every cell,
/// and report how the columns fit `area_width`.
///
/// A box wider than the area cuts its action text to fit, so every key stays
/// on screen. If the keys leave too little room for text, the columns keep
/// their text whole and lay out too wide.
fn lay_out_columns(rows: &[HintRow], rows_cap: usize, area_width: u16) -> (Vec<LaidColumn>, Fit) {
    let col_count = rows.len().div_ceil(rows_cap);
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

    let caps = action_caps(&key_widths, &action_widths, area_width as usize);
    let fit = match &caps {
        Some(caps) if caps == &action_widths => Fit::Whole,
        Some(_) => Fit::Cut,
        None => Fit::TooWide,
    };
    let caps = caps.unwrap_or_else(|| action_widths.clone());

    let columns = chunks
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
                    submenu: row.submenu,
                })
                .collect();
            LaidColumn {
                cells,
                key_width,
                action_width: action_widths[i].min(cap),
            }
        })
        .collect();
    (columns, fit)
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
                submenu: action.starts_with('+'),
            });
        }
    }
    rows
}

/// The subtitle of a box that lists `count` rows, such as `22 keys`.
pub(crate) fn key_count(count: usize) -> String {
    match count {
        1 => "1 key".to_string(),
        count => format!("{count} keys"),
    }
}

/// The rows of `hints`, with the chars that share a label on one row, and then
/// the Esc row that cancels the chord.
pub(crate) fn chord_rows(hints: &ChordHints) -> Vec<HintRow> {
    let bindings: Vec<(&str, String, Option<Toggle>)> = hints
        .rows
        .iter()
        .chain([&CANCEL_ROW])
        .map(|&(chars, label)| (chars, label.to_string(), None))
        .collect();
    group_by_action(&bindings)
}

#[cfg(test)]
mod tests {
    use super::{
        action_caps, group_by_action, lay_out, render_hints_grouped, FooterKeys, HintRow,
        HintsCache, HintsFooter, LaidCell, LayoutKey,
    };
    use crate::{
        action_handlers,
        keymap::CompiledKey,
        test_harness::TestHarness,
        theme::Theme,
        toggle::{Toggle, ToggleStates},
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };

    fn row_text(buf: &Buffer, y: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn render(bindings: &[(&str, String)], width: u16, height: u16) -> Buffer {
        let mut cache =
            HintsCache::new(0, group_by_action(&plain(bindings)), FooterKeys::default());
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
            "",
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

        // A column holds eight rows, so the second one starts at k08.
        let side_by_side = (0..buf.area.height).any(|y| {
            let text = row_text(&buf, y);
            text.contains("k00") && text.contains("k08")
        });
        assert!(
            side_by_side,
            "the first rows of two columns share a buffer row",
        );
    }

    /// The column heights of `count` numbered rows whose labels are `label`
    /// long, laid out over an area `width` by `height`.
    fn column_heights(count: usize, label: usize, width: u16, height: u16) -> Vec<usize> {
        let rows: Vec<HintRow> = (0..count)
            .map(|i| HintRow {
                keys: format!("k{i:02}"),
                action: "x".repeat(label),
                toggle: None,
                submenu: false,
            })
            .collect();
        let key = LayoutKey {
            rows: 0,
            area_width: width,
            area_height: height,
            title_len: 5,
            subtitle_len: 7,
            footer_len: 0,
            right_len: 0,
        };
        lay_out(&rows, key)
            .expect("the area holds a row")
            .columns
            .iter()
            .map(|column| column.cells.len())
            .collect()
    }

    /// A column holds eight rows at most, so a long list spreads over three
    /// columns rather than one column the window's height.
    #[test]
    fn twenty_two_rows_lay_out_as_three_columns_of_eight() {
        assert_eq!(column_heights(22, 20, 160, 50), [8, 8, 6]);
    }

    /// Where columns of eight cut their labels to fit, the shortest taller
    /// columns that keep every label whole take their place.
    #[test]
    fn a_narrow_area_takes_the_shortest_columns_that_keep_labels_whole() {
        assert_eq!(column_heights(22, 20, 70, 50), [11, 11]);
    }

    /// The padded strings are what a repaint is meant to stop rebuilding, so an
    /// unchanged frame has to reach the paint without touching them.
    #[test]
    fn an_unchanged_frame_paints_from_the_laid_out_strings() {
        let bindings = vec![("k", "act".to_string()), ("kk", "other".to_string())];
        let mut cache =
            HintsCache::new(7, group_by_action(&plain(&bindings)), FooterKeys::default());

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
        let mut cache =
            HintsCache::new(7, group_by_action(&plain(&bindings)), FooterKeys::default());

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

    #[test]
    fn a_footer_wider_than_the_area_is_cut_to_the_box() {
        let bindings = vec![("k", "act".to_string())];
        let mut cache =
            HintsCache::new(0, group_by_action(&plain(&bindings)), FooterKeys::default());
        let footer = HintsFooter {
            text: "x".repeat(100),
            style: Default::default(),
        };

        let buf = render_into(&mut cache, Some(&footer), ToggleStates::default(), 40, 10);

        let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        let shows = |needle: &str| rows.iter().any(|row| row.contains(needle));
        assert_eq!(
            (
                shows(" k   act"),
                shows(&format!(" {}\u{2026}", "x".repeat(35)))
            ),
            (true, true),
            "the box shows its key, and the footer ends in an ellipsis 35 columns in"
        );
    }

    #[test]
    fn a_footer_is_measured_in_chars() {
        let bindings = vec![("k", "act".to_string())];
        let mut cache =
            HintsCache::new(0, group_by_action(&plain(&bindings)), FooterKeys::default());
        let footer = HintsFooter {
            text: "\u{b7}".repeat(20),
            style: Default::default(),
        };

        render_into(&mut cache, Some(&footer), ToggleStates::default(), 60, 20);

        assert_eq!(
            cache.layout.as_ref().expect("laid out").box_width,
            24,
            "twenty two-byte dots take twenty columns, plus the frame and padding"
        );
    }

    /// The footer names the hide and help keys at its right, past its own
    /// text, and the box widens to hold both.
    #[test]
    fn the_footer_keys_sit_right_of_the_footer_text() {
        let bindings = vec![("k", "act".to_string())];
        let keys = FooterKeys {
            hide: Some("C-?".to_string()),
            help: Some("?".to_string()),
        };
        let mut cache = HintsCache::new(0, group_by_action(&plain(&bindings)), keys);
        let footer = HintsFooter {
            text: "1/9".to_string(),
            style: Default::default(),
        };
        let buf = render_into(&mut cache, Some(&footer), ToggleStates::default(), 60, 20);

        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| row_text(&buf, y).trim().to_string())
            .collect();
        assert!(
            rows.contains(&"│ 1/9   C-? hide \u{b7} ? help │".to_string()),
            "the footer row reads its text, then the keys: {rows:#?}",
        );
    }

    /// A footer text too long for the box stops short of the keys at its right,
    /// so the keys stay whole.
    #[test]
    fn a_long_footer_text_stops_short_of_the_footer_keys() {
        let bindings = vec![("k", "act".to_string())];
        let keys = FooterKeys {
            hide: Some("C-?".to_string()),
            help: Some("?".to_string()),
        };
        let mut cache = HintsCache::new(0, group_by_action(&plain(&bindings)), keys);
        let footer = HintsFooter {
            text: "x".repeat(100),
            style: Default::default(),
        };
        let buf = render_into(&mut cache, Some(&footer), ToggleStates::default(), 40, 10);

        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| row_text(&buf, y).trim().to_string())
            .collect();
        let cut = format!("│ {}\u{2026}   C-? hide \u{b7} ? help │", "x".repeat(15));
        assert!(
            rows.contains(&cut),
            "the text ends in an ellipsis three columns short of the keys: {rows:#?}",
        );
    }

    /// The title row names the box at its left and the subtitle at its right,
    /// each set into a cell frame's border with a space either side.
    #[test]
    fn the_title_row_holds_the_title_and_the_subtitle() {
        let bindings = vec![("k", "act".to_string())];
        let mut cache =
            HintsCache::new(0, group_by_action(&plain(&bindings)), FooterKeys::default());
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        render_hints_grouped(
            "normal",
            "1 key",
            &mut cache,
            None,
            ToggleStates::default(),
            &Theme::empty(),
            area,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| row_text(&buf, y).trim().to_string())
            .collect();
        assert!(
            rows.contains(&"┌ normal ─ 1 key ┐".to_string()),
            "the border row holds both texts: {rows:#?}",
        );
    }

    /// The theme `t` that `body` defines.
    fn theme_t(body: &str) -> Theme {
        let (config, errors) = stoat_config::parse(&format!("theme t {{ {body} }}"));
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Theme::from_config(&config.expect("the theme parses"), "t").expect("theme t")
    }

    /// Paint `bindings` in `theme` as cells, on a scene that draws no
    /// components.
    fn render_cells(bindings: &[(&str, String, Option<Toggle>)], theme: &Theme) -> Buffer {
        let mut cache = HintsCache::new(0, group_by_action(bindings), FooterKeys::default());
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(false);
        render_hints_grouped(
            "normal",
            "",
            &mut cache,
            None,
            ToggleStates::default(),
            theme,
            area,
            &mut buf,
            &mut scene,
        );
        buf
    }

    /// A row's key draws bold and its action at the text's weight, as the
    /// HintBox board draws them.
    #[test]
    fn a_row_draws_its_key_bold() {
        let theme = theme_t(r##"ui.key_label.fg = "#010203";"##);
        let buf = render_cells(&plain(&[("k", "act".to_string())]), &theme);

        let y = (0..buf.area.height)
            .find(|&y| row_text(&buf, y).contains("act"))
            .expect("the row");
        let bold = |needle: &str| {
            (0..buf.area.width)
                .find(|&x| buf[(x, y)].symbol() == needle)
                .map(|x| buf[(x, y)].modifier.contains(Modifier::BOLD))
        };
        assert_eq!((bold("k"), bold("a")), (Some(true), Some(false)));
    }

    /// A cell frame's border rules the title row off by itself, so the row's
    /// dashes keep the frame's color rather than taking the card hairline's.
    #[test]
    fn a_cell_frame_keeps_its_border_color_across_the_title_row() {
        let theme = theme_t(
            r##"ui.modal.hints.card = { fg: "#010203", bg: "#040506" };
                ui.border.inactive.fg = "#0a0b0c";"##,
        );
        let buf = render_cells(&plain(&[("k", "act".to_string())]), &theme);

        let top = (0..buf.area.height)
            .find(|&y| row_text(&buf, y).contains('┌'))
            .expect("a cell frame");
        assert_eq!(dash_colors(&buf, top), [Color::Rgb(1, 2, 3)]);
    }

    /// A box whose footer names nothing ends at its last row, so its bottom
    /// border follows that row and keeps the frame's color, with no footer
    /// separator over it.
    #[test]
    fn a_footerless_box_ends_at_its_last_row() {
        let theme = theme_t(
            r##"ui.modal.hints.card = { fg: "#010203", bg: "#040506" };
                ui.border.inactive.fg = "#0a0b0c";"##,
        );
        let buf = render_cells(&plain(&[("k", "act".to_string())]), &theme);

        let bottom = (0..buf.area.height)
            .find(|&y| row_text(&buf, y).contains('└'))
            .expect("a cell frame");
        assert_eq!(
            (
                row_text(&buf, bottom - 1).contains("act"),
                dash_colors(&buf, bottom)
            ),
            (true, vec![Color::Rgb(1, 2, 3)]),
        );
    }

    /// The colors of the `─` cells across row `y`, each run of one color once.
    fn dash_colors(buf: &Buffer, y: u16) -> Vec<Color> {
        let mut colors: Vec<Color> = (buf.area.x..buf.area.x + buf.area.width)
            .filter(|&x| buf[(x, y)].symbol() == "─")
            .map(|x| buf[(x, y)].fg)
            .collect();
        colors.dedup();
        colors
    }

    /// A submenu row's label takes the submenu style. A theme that sets none
    /// paints it in the keys' color, so it still stands apart from the plain
    /// labels.
    #[test]
    fn a_submenu_label_takes_the_submenu_style() {
        let label_colors = |body: &str| {
            let bindings = plain(&[("b", "+buffer".to_string()), ("k", "zzz".to_string())]);
            let buf = render_cells(&bindings, &theme_t(body));
            let fg_of = |glyph: &str| {
                buf.content
                    .iter()
                    .find(|cell| cell.symbol() == glyph)
                    .map(|cell| cell.fg)
            };
            (fg_of("+"), fg_of("z"))
        };
        let colors = r##"ui.text.fg = "#010101"; ui.key_label.fg = "#020202";"##;
        assert_eq!(
            [
                label_colors(&format!(r##"{colors} ui.hints.submenu.fg = "#030303";"##)),
                label_colors(colors),
            ],
            [
                (Some(Color::Rgb(3, 3, 3)), Some(Color::Rgb(1, 1, 1))),
                (Some(Color::Rgb(2, 2, 2)), Some(Color::Rgb(1, 1, 1))),
            ],
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
            let mut cache = HintsCache::new(0, group_by_action(&bindings), FooterKeys::default());
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
                on.contains("f\u{25cf}  follow"),
                off.contains("f\u{25cb}  follow"),
                unmarked.contains("f   follow"),
            ),
            (true, true, true),
            "the mark sits in the cell after the key:\n{on}\n{off}\n{unmarked}",
        );
        assert_eq!(
            (on.replace('\u{25cf}', " "), off.replace('\u{25cb}', " ")),
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
            ('6', "underline changes"),
            ('7', "bold changes"),
            ('8', "syntax colors"),
            ('9', "tint down"),
            ('0', "tint up"),
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
