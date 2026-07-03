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

    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let action_width = rows.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let gap = 3;
    let bindings_width = key_width + gap + action_width;
    let border_pad = 2;
    let title_width = mode.len() + 4;
    let footer_width = footer.map(|f| f.text.len()).unwrap_or(0);
    let content_width = bindings_width.max(title_width).max(footer_width);
    let extra_rows = footer.map(|_| 2).unwrap_or(0);
    let box_width = (content_width + border_pad) as u16;
    let box_height = (rows.len() + border_pad + extra_rows) as u16;

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

    for (i, (key, action)) in rows.iter().enumerate() {
        let row = inner.y + i as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let padded_key = format!("{key:>width$}", width = key_width);
        crate::render::chrome::text(
            buf,
            inner.x,
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
            inner.x + key_width as u16,
            row,
            end_x,
            &action_text,
            action_style,
            run_bg,
            HINT_TEXT_SCALE,
            scene.as_deref_mut(),
        );
    }

    if let Some(footer) = footer {
        let sep_row = inner.y + rows.len() as u16;
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
                inner.x,
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
