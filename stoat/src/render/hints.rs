use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Clear, Widget},
};
use std::collections::HashMap;

/// Hint-row and footer text size under stoatty, in 256ths of a cell (0.85x).
const HINT_TEXT_SCALE: u16 = 218;

pub(crate) struct HintsFooter {
    pub(crate) text: String,
    pub(crate) style: Style,
}

pub(crate) fn render_hints(
    mode: &str,
    bindings: &[(&str, String)],
    footer: Option<&HintsFooter>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    if bindings.is_empty() || area.width < 10 || area.height < 4 {
        return;
    }

    let rows = group_by_action(bindings);

    let gap = 3;
    let inter_col_gap = 3;
    let border_pad = 2;
    let content_pad = 2;
    let extra_rows = footer.map(|_| 2).unwrap_or(0);

    // Rows that fit vertically inside the box. The layout grows into extra
    // columns once the bindings would overflow this height.
    let available_rows = (area.height as usize).saturating_sub(border_pad + extra_rows);
    if available_rows == 0 {
        return;
    }

    let col_count = rows.len().div_ceil(available_rows);
    let rows_per_col = rows.len().div_ceil(col_count);

    let columns: Vec<_> = rows
        .chunks(rows_per_col)
        .map(|chunk| {
            let key_width = chunk.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            let action_width = chunk.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
            (chunk, key_width, action_width)
        })
        .collect();

    let max_col_rows = columns.iter().map(|(c, _, _)| c.len()).max().unwrap_or(0);
    let columns_width: usize = columns
        .iter()
        .map(|(_, kw, aw)| kw + gap + aw)
        .sum::<usize>()
        + inter_col_gap * columns.len().saturating_sub(1);

    let title_width = mode.len() + 4;
    let footer_width = footer.map(|f| f.text.len()).unwrap_or(0);
    let content_width = columns_width.max(title_width).max(footer_width);
    let box_width = (content_width + border_pad + content_pad) as u16;
    let box_height = (max_col_rows + border_pad + extra_rows) as u16;

    if box_width > area.width || box_height > area.height {
        return;
    }

    let x = area.x + area.width.saturating_sub(box_width);
    let y = area.y + area.height.saturating_sub(box_height);
    let help_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let title = format!(" {mode} ");
    Clear.render(help_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        help_area,
        Some(title.as_str()),
        modal_style,
        theme,
        scene.as_deref_mut(),
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
    for &(chunk, key_width, action_width) in &columns {
        for (i, (key, action)) in chunk.iter().enumerate() {
            let row = inner.y + i as u16;
            if row >= inner.y + inner.height {
                break;
            }
            let padded_key = format!("{key:>width$}", width = key_width);
            crate::render::chrome::text(
                buf,
                col_x,
                row,
                end_x,
                &padded_key,
                key_style,
                run_bg,
                HINT_TEXT_SCALE,
                scene.as_deref_mut(),
            );

            let action_text = format!("   {action}");
            crate::render::chrome::text(
                buf,
                col_x + key_width as u16,
                row,
                end_x,
                &action_text,
                action_style,
                run_bg,
                HINT_TEXT_SCALE,
                scene.as_deref_mut(),
            );
        }
        col_x += (key_width + gap + action_width + inter_col_gap) as u16;
    }

    if let Some(footer) = footer {
        let sep_row = inner.y + max_col_rows as u16;
        let text_row = sep_row + 1;
        if sep_row < inner.y + inner.height {
            let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            crate::render::chrome::hline(
                buf,
                inner.x,
                sep_row,
                inner.width,
                sep_style,
                scene.as_deref_mut(),
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
                HINT_TEXT_SCALE,
                scene,
            );
        }
    }
}

/// Collapses entries that share an action description, joining their keys with
/// `", "` in first-seen order. Ensures each action appears on exactly one row.
fn group_by_action<'a>(bindings: &'a [(&str, String)]) -> Vec<(String, &'a str)> {
    let mut rows: Vec<(String, &'a str)> = Vec::new();
    let mut index: HashMap<&'a str, usize> = HashMap::new();
    for (key, action) in bindings {
        let action = action.as_str();
        if let Some(&i) = index.get(action) {
            let row = &mut rows[i];
            row.0.push_str(", ");
            row.0.push_str(key);
        } else {
            index.insert(action, rows.len());
            rows.push((key.to_string(), action));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::render_hints;
    use crate::theme::Theme;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn row_text(buf: &Buffer, y: u16) -> String {
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn render(bindings: &[(&str, String)], width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render_hints(
            "normal",
            bindings,
            None,
            &Theme::empty(),
            area,
            &mut buf,
            None,
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

    #[test]
    fn box_too_wide_for_the_area_renders_nothing() {
        let keys: Vec<String> = (0..40).map(|i| format!("k{i:02}")).collect();
        let buf = render(&numbered_bindings(&keys), 30, 5);

        let painted = (0..buf.area.height).any(|y| !row_text(&buf, y).trim().is_empty());
        assert!(!painted, "a box too wide for the area paints nothing");
    }
}
